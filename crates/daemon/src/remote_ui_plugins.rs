//! Durable, fail-closed installation records for Remote UI component packages.
//!
//! The daemon never serializes [`InstalledPlugin`] itself.  Its private fields
//! are an intentional security boundary.  Instead this module persists a
//! daemon-MACed description of the inputs needed to rebuild that value, then
//! calls the public lifecycle API and re-verifies the content-addressed archive
//! every time a launch descriptor is minted.  A forged JSON record therefore
//! cannot manufacture either a grant or an enabled `InstalledPlugin`.

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use codypendent_protocol::{SessionId, UiCapabilities};
use codypendent_sandbox::{
    checksum_of, CapabilitiesSpec, Capability, CapabilitySet, InstalledPlugin, LifecycleError,
    LifecycleState, PluginKind, PluginManifest, TrustedPublishers, UiCapability, UiTarget,
    UnsignedPolicy,
};
use codypendent_ui_host::{
    UiWorker, UiWorkerConfig, UiWorkerLaunch, UiWorkerLaunchPurpose, UiWorkerRuntime,
    UiWorkerSignal, UiWorkerSupervisor,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::remote_ui::RemoteUiBroker;
use crate::remote_ui_workers::VerifiedUiLaunchSource;

const RECORD_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;
// The archive/package safety limits that used to live here now belong to
// `codypendent_sandbox::package`, which owns extraction. Duplicating them here
// let the two drift.
const MAX_PACKAGE_ARCHIVE_BYTES: usize = 10 * 1024 * 1024;
const MAX_PLUGIN_MEMORY_MB: u64 = 2_048;
const MAX_PLUGIN_CPU_SECONDS: u64 = 300;
const MAX_PLUGIN_WALL_SECONDS: u64 = 3_600;
const MAX_PLUGIN_OUTPUT_MB: u64 = 64;
const RUNTIME_SEAL_FILE: &str = ".codypendent-runtime-seal.json";
const MAX_RUNTIME_SEAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RUNTIME_ENTRIES: usize = 100_000;
const MAX_RUNTIME_BYTES: u64 = 2 * 1024 * 1024 * 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum RemoteUiPluginStoreError {
    #[error("Remote UI plugin store I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Remote UI plugin record is invalid: {0}")]
    Record(String),
    #[error("Remote UI plugin record authentication failed")]
    Authentication,
    #[error("Remote UI package extraction failed: {0}")]
    Package(String),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error(transparent)]
    Worker(#[from] codypendent_ui_host::UiWorkerError),
    #[error(transparent)]
    Trust(#[from] codypendent_sandbox::TrustStoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("plugin `{0}` is already installed")]
    AlreadyInstalled(String),
    #[error("plugin `{0}` is not installed")]
    NotInstalled(String),
    #[error("plugin `{0}` already has an update awaiting approval")]
    UpdatePending(String),
    #[error("plugin update did not expand permissions and therefore needs no approval")]
    ApprovalNotRequired,
    #[error("the update approval receipt is invalid or already consumed")]
    ApprovalReceiptMismatch,
    #[error("scope `{scope}` requires a concrete session binding")]
    SessionBindingRequired { scope: String },
    #[error("scope `{scope}` does not accept a session binding")]
    UnexpectedSessionBinding { scope: String },
    #[error("plugin `{0}` is busy in another lifecycle transaction")]
    Busy(String),
    #[error("plugin record changed concurrently (expected revision {expected}, found {actual})")]
    ConcurrentUpdate { expected: u64, actual: u64 },
    #[error("plugin resource cap `{field}` exceeds the host maximum {maximum}")]
    ResourceLimit { field: &'static str, maximum: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedRecord {
    payload: StoredPlugin,
    mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPlugin {
    record_version: u32,
    revision: u64,
    manifest: PluginManifest,
    artifact_checksum: String,
    publisher_key_checksum: Option<String>,
    allow_unsigned: bool,
    granted: CapabilitiesSpec,
    granted_ui: BTreeSet<UiCapability>,
    lifecycle: StoredLifecycle,
    enabled_scope: Option<EnabledScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_update: Option<StoredPendingUpdate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    consumed_update_receipts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StoredLifecycle {
    InstalledDisabled,
    SmokeTested,
    Enabled,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledScope {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPendingUpdate {
    approval_receipt: String,
    permission_diff: String,
    candidate: PluginManifest,
    artifact_checksum: String,
    publisher_key_checksum: Option<String>,
    allow_unsigned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeSealEntry {
    kind: String,
    path: String,
    digest: String,
}

/// Non-secret status returned to management surfaces.  No constructor for an
/// executable lifecycle value is exposed through this view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUiPluginStatus {
    pub id: String,
    pub version: String,
    pub state: LifecycleState,
    pub enabled_scope: Option<String>,
    pub update_approval_receipt: Option<String>,
    pub update_permission_diff: Option<String>,
}

/// Publisher trust is the primary execution gate. Once its durable removal
/// succeeds every matching plugin is effectively revoked even if an individual
/// lifecycle record cannot be rewritten; `failures` reports those repair
/// errors while `plugins` lets the daemon synchronously tear down all authority.
#[derive(Debug, Clone)]
pub struct PublisherTrustRemoval {
    pub plugins: Vec<RemoteUiPluginStatus>,
    pub failures: Vec<String>,
}

/// Durable content-addressed package store and verified launch source.
#[derive(Debug, Clone)]
pub struct RemoteUiPluginStore {
    root: PathBuf,
    trust_store: PathBuf,
    runtime: UiWorkerRuntime,
    record_key: Arc<Vec<u8>>,
    local_locks: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>>,
    trust_lock: Arc<Mutex<()>>,
}

impl RemoteUiPluginStore {
    pub fn open(
        data_dir: impl AsRef<Path>,
        config_dir: impl AsRef<Path>,
        runtime: UiWorkerRuntime,
        record_key: Vec<u8>,
    ) -> Result<Self, RemoteUiPluginStoreError> {
        if record_key.len() < 32 {
            return Err(RemoteUiPluginStoreError::Record(
                "record authentication key must contain at least 32 bytes".into(),
            ));
        }
        let root = data_dir.as_ref().join("plugins").join("remote-ui");
        create_private_dir(&root)?;
        create_private_dir(&root.join("records"))?;
        create_private_dir(&root.join("records/archive"))?;
        create_private_dir(&root.join("artifacts"))?;
        create_private_dir(&root.join("packages"))?;
        create_private_dir(&root.join("tmp"))?;
        create_private_dir(&root.join("locks"))?;
        let store = Self {
            root,
            trust_store: config_dir.as_ref().join("trusted_publishers.toml"),
            runtime,
            record_key: Arc::new(record_key),
            local_locks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            trust_lock: Arc::new(Mutex::new(())),
        };
        store.sweep_orphan_temps()?;
        Ok(store)
    }

    /// Verify, store, and extract a component package.  It remains inert.
    pub fn install_disabled(
        &self,
        manifest: PluginManifest,
        artifact: &[u8],
        allow_unsigned: bool,
        granted: CapabilitySet,
        granted_ui: BTreeSet<UiCapability>,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        let _trust = self
            .trust_lock
            .lock()
            .expect("publisher trust mutex poisoned");
        if artifact.len() > MAX_PACKAGE_ARCHIVE_BYTES {
            return Err(RemoteUiPluginStoreError::Package(format!(
                "compressed package exceeds {} bytes",
                MAX_PACKAGE_ARCHIVE_BYTES
            )));
        }
        let local = self.local_lock(&manifest.id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(&manifest.id)?;
        validate_resource_limits(&manifest)?;
        if !manifest_has_remote_ui(&manifest) {
            return Err(RemoteUiPluginStoreError::Record(format!(
                "plugin `{}` does not declare a governed UI component",
                manifest.id
            )));
        }
        let replaced_revoked = if self.record_path(&manifest.id).exists() {
            let record = self.read_record(&manifest.id)?;
            if record.manifest == manifest
                && record.artifact_checksum == checksum_of(artifact)
                && record.granted == capability_spec(&granted)
                && record.granted_ui == granted_ui
                && record.allow_unsigned == allow_unsigned
            {
                return Ok(status(&record));
            }
            if record.lifecycle != StoredLifecycle::Revoked {
                return Err(RemoteUiPluginStoreError::AlreadyInstalled(manifest.id));
            }
            Some(record)
        } else {
            None
        };
        let key = self.publisher_key(&manifest)?;
        let unsigned = unsigned_policy(allow_unsigned);
        let installed = InstalledPlugin::install_disabled(
            manifest,
            artifact,
            key.as_deref(),
            unsigned,
            granted,
            granted_ui,
        )?;
        self.store_package(installed.content_hash(), artifact)?;
        let mut record =
            self.record_from_installed(&installed, allow_unsigned, key.as_deref(), None)?;
        if let Some(previous) = replaced_revoked {
            let previous_path = self.record_path(&previous.manifest.id);
            let previous_bytes =
                std::fs::read(&previous_path).map_err(|source| RemoteUiPluginStoreError::Io {
                    path: previous_path.clone(),
                    source,
                })?;
            let archive = self.root.join("records/archive").join(format!(
                "{}-revision-{}-{}.json",
                hex::encode(Sha256::digest(previous.manifest.id.as_bytes())),
                previous.revision,
                Uuid::now_v7()
            ));
            atomic_write_once(&archive, &previous_bytes)?;
            record.revision = previous.revision + 1;
            self.write_record(&record, Some(previous.revision))?;
        } else {
            self.write_record(&record, None)?;
        }
        Ok(status(&record))
    }

    /// Run the real worker handshake in the enforcing sandbox and only then
    /// persist the smoke-tested lifecycle transition.
    pub async fn smoke_test(
        &self,
        plugin_id: &str,
        supervisor: Arc<UiWorkerSupervisor>,
        host_offer: UiCapabilities,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        let local = self.local_lock(plugin_id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(plugin_id)?;
        let mut record = self.read_record(plugin_id)?;
        if matches!(
            record.lifecycle,
            StoredLifecycle::SmokeTested | StoredLifecycle::Enabled
        ) && record.pending_update.is_none()
        {
            return Ok(status(&record));
        }
        if record.pending_update.is_some() {
            return Err(RemoteUiPluginStoreError::UpdatePending(plugin_id.into()));
        }
        let artifact = self.read_artifact(&record.artifact_checksum)?;
        let mut installed = self.rehydrate(&record, &artifact)?;
        if installed.state() != LifecycleState::InstalledDisabled {
            return Err(LifecycleError::IllegalTransition {
                action: "smoke-test",
                state: installed.state(),
            }
            .into());
        }
        let expected = record.revision;
        // Do not hold a blocking mutex while awaiting a real process. The
        // record revision is the optimistic transaction token; a concurrent
        // revoke/update makes the final CAS fail instead of being overwritten.
        drop(_file);
        drop(_local);
        for target in launch_targets(installed.manifest()) {
            let launch = UiWorkerLaunch::from_installed_with_runtime(
                &installed,
                &artifact,
                self.package_path(&record.artifact_checksum)?,
                target,
                self.runtime.clone(),
                UiWorkerLaunchPurpose::SmokeTest,
            )?;
            let mut worker = supervisor
                .launch(launch.clone(), host_offer.clone())
                .await?;
            preflight_initial_ui(&launch, &mut worker).await?;
            worker.shutdown().await?;
        }
        installed.mark_smoke_tested()?;
        let local = self.local_lock(plugin_id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(plugin_id)?;
        record.lifecycle = StoredLifecycle::SmokeTested;
        record.enabled_scope = None;
        record.revision = expected + 1;
        self.write_record(&record, Some(expected))?;
        Ok(status(&record))
    }

    /// Re-run initialization for the exact sealed approval candidate before
    /// consuming its receipt or changing the enabled record.
    pub async fn preflight_pending_update(
        &self,
        plugin_id: &str,
        approval_receipt: &str,
        supervisor: Arc<UiWorkerSupervisor>,
        host_offer: UiCapabilities,
    ) -> Result<(), RemoteUiPluginStoreError> {
        let (installed, artifact, package) = {
            let _trust = self
                .trust_lock
                .lock()
                .expect("publisher trust mutex poisoned");
            let local = self.local_lock(plugin_id);
            let _local = local.lock().expect("plugin lifecycle mutex poisoned");
            let _file = self.lock_plugin(plugin_id)?;
            let record = self.read_record(plugin_id)?;
            let pending = record
                .pending_update
                .as_ref()
                .ok_or(RemoteUiPluginStoreError::ApprovalNotRequired)?;
            if pending.approval_receipt != approval_receipt {
                return Err(RemoteUiPluginStoreError::ApprovalReceiptMismatch);
            }
            let artifact = self.read_artifact(&pending.artifact_checksum)?;
            let key = self.publisher_key(&pending.candidate)?;
            if key.as_deref().map(checksum_of) != pending.publisher_key_checksum {
                return Err(RemoteUiPluginStoreError::Authentication);
            }
            let mut installed = InstalledPlugin::install_disabled(
                pending.candidate.clone(),
                &artifact,
                key.as_deref(),
                unsigned_policy(pending.allow_unsigned),
                CapabilitySet::from_spec(&pending.candidate.capabilities),
                pending
                    .candidate
                    .ui
                    .as_ref()
                    .ok_or_else(|| {
                        RemoteUiPluginStoreError::Record(
                            "pending candidate has no governed UI declaration".into(),
                        )
                    })?
                    .requested_capabilities
                    .iter()
                    .copied()
                    .collect(),
            )?;
            let package = self.package_path(&pending.artifact_checksum)?;
            installed.mark_smoke_tested()?;
            (installed, artifact, package)
        };
        self.preflight_installed(&installed, &artifact, &package, supervisor, host_offer)
            .await
    }

    async fn preflight_installed(
        &self,
        installed: &InstalledPlugin,
        artifact: &[u8],
        package: &Path,
        supervisor: Arc<UiWorkerSupervisor>,
        host_offer: UiCapabilities,
    ) -> Result<(), RemoteUiPluginStoreError> {
        for target in launch_targets(installed.manifest()) {
            let launch = UiWorkerLaunch::from_installed_with_runtime(
                installed,
                artifact,
                package,
                target,
                self.runtime.clone(),
                UiWorkerLaunchPurpose::SmokeTest,
            )?;
            let mut worker = supervisor
                .launch(launch.clone(), host_offer.clone())
                .await?;
            preflight_initial_ui(&launch, &mut worker).await?;
            worker.shutdown().await?;
        }
        Ok(())
    }

    pub fn enable(
        &self,
        plugin_id: &str,
        scope: &str,
        session_id: Option<SessionId>,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        let _trust = self
            .trust_lock
            .lock()
            .expect("publisher trust mutex poisoned");
        let local = self.local_lock(plugin_id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(plugin_id)?;
        let mut record = self.read_record(plugin_id)?;
        if record.pending_update.is_some() {
            return Err(RemoteUiPluginStoreError::UpdatePending(plugin_id.into()));
        }
        let is_global = matches!(scope, "user" | "global");
        if !is_global && session_id.is_none() {
            return Err(RemoteUiPluginStoreError::SessionBindingRequired {
                scope: scope.into(),
            });
        }
        if is_global && session_id.is_some() {
            return Err(RemoteUiPluginStoreError::UnexpectedSessionBinding {
                scope: scope.into(),
            });
        }
        if record.lifecycle == StoredLifecycle::Enabled
            && record
                .enabled_scope
                .as_ref()
                .is_some_and(|enabled| enabled.name == scope && enabled.session_id == session_id)
        {
            return Ok(status(&record));
        }
        let artifact = self.read_artifact(&record.artifact_checksum)?;
        let mut installed = self.rehydrate(&record, &artifact)?;
        installed.enable(scope)?;
        record.lifecycle = StoredLifecycle::Enabled;
        record.enabled_scope = Some(EnabledScope {
            name: scope.into(),
            session_id,
        });
        let expected = record.revision;
        record.revision = expected + 1;
        self.write_record(&record, Some(expected))?;
        Ok(status(&record))
    }

    pub fn revoke(
        &self,
        plugin_id: &str,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        let local = self.local_lock(plugin_id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(plugin_id)?;
        let mut record = self.read_record(plugin_id)?;
        if record.lifecycle == StoredLifecycle::Revoked {
            return Ok(status(&record));
        }
        record.lifecycle = StoredLifecycle::Revoked;
        record.enabled_scope = None;
        record.pending_update = None;
        let expected = record.revision;
        record.revision = expected + 1;
        self.write_record(&record, Some(expected))?;
        Ok(status(&record))
    }

    /// Verify and stage an update without executing its code. Safe/narrowing
    /// candidates receive an internal receipt; expanding candidates receive a
    /// one-shot human approval receipt.
    pub fn update(
        &self,
        plugin_id: &str,
        candidate: PluginManifest,
        artifact: &[u8],
        allow_unsigned: bool,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        let _trust = self
            .trust_lock
            .lock()
            .expect("publisher trust mutex poisoned");
        if artifact.len() > MAX_PACKAGE_ARCHIVE_BYTES {
            return Err(RemoteUiPluginStoreError::Package(format!(
                "compressed package exceeds {} bytes",
                MAX_PACKAGE_ARCHIVE_BYTES
            )));
        }
        let local = self.local_lock(plugin_id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(plugin_id)?;
        validate_resource_limits(&candidate)?;
        let mut record = self.read_record(plugin_id)?;
        let expected_revision = record.revision;
        if record.manifest == candidate
            && record.artifact_checksum == checksum_of(artifact)
            && record.pending_update.is_none()
        {
            return Ok(status(&record));
        }
        if record.pending_update.as_ref().is_some_and(|pending| {
            pending.candidate == candidate
                && pending.artifact_checksum == checksum_of(artifact)
                && pending.allow_unsigned == allow_unsigned
        }) {
            return Ok(status(&record));
        }
        if record.pending_update.is_some() {
            return Err(RemoteUiPluginStoreError::UpdatePending(plugin_id.into()));
        }
        let current_artifact = self.read_artifact(&record.artifact_checksum)?;
        let mut installed = self.rehydrate(&record, &current_artifact)?;
        let key = self.publisher_key(&candidate)?;
        match installed.update(
            candidate.clone(),
            artifact,
            key.as_deref(),
            unsigned_policy(allow_unsigned),
        ) {
            Ok(_) => {
                self.store_package(candidate.security.checksum.trim(), artifact)?;
                record.pending_update = Some(StoredPendingUpdate {
                    approval_receipt: format!("safe-{}", Uuid::now_v7()),
                    permission_diff: String::new(),
                    artifact_checksum: candidate.security.checksum.trim().into(),
                    publisher_key_checksum: key.as_deref().map(checksum_of),
                    allow_unsigned,
                    candidate,
                });
                record.revision = expected_revision + 1;
                self.write_record(&record, Some(expected_revision))?;
                Ok(status(&record))
            }
            Err(LifecycleError::UpdateExpandsPermissions {
                diff,
                approval_receipt,
            }) => {
                self.store_package(candidate.security.checksum.trim(), artifact)?;
                record.pending_update = Some(StoredPendingUpdate {
                    approval_receipt,
                    permission_diff: diff,
                    artifact_checksum: candidate.security.checksum.trim().into(),
                    publisher_key_checksum: key.as_deref().map(checksum_of),
                    allow_unsigned,
                    candidate,
                });
                record.revision = expected_revision + 1;
                self.write_record(&record, Some(expected_revision))?;
                Ok(status(&record))
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn approve_update(
        &self,
        plugin_id: &str,
        approval_receipt: &str,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        let _trust = self
            .trust_lock
            .lock()
            .expect("publisher trust mutex poisoned");
        let local = self.local_lock(plugin_id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(plugin_id)?;
        let record = self.read_record(plugin_id)?;
        let expected_revision = record.revision;
        let pending = match record.pending_update.as_ref() {
            Some(pending) => pending,
            None if record
                .consumed_update_receipts
                .iter()
                .any(|receipt| receipt == approval_receipt) =>
            {
                return Ok(status(&record))
            }
            None => return Err(RemoteUiPluginStoreError::ApprovalNotRequired),
        };
        if pending.approval_receipt != approval_receipt {
            return Err(RemoteUiPluginStoreError::ApprovalReceiptMismatch);
        }
        let current_artifact = self.read_artifact(&record.artifact_checksum)?;
        let candidate_artifact = self.read_artifact(&pending.artifact_checksum)?;
        let mut installed = self.rehydrate(&record, &current_artifact)?;
        let key = self.publisher_key(&pending.candidate)?;
        if key.as_deref().map(checksum_of) != pending.publisher_key_checksum {
            return Err(RemoteUiPluginStoreError::Authentication);
        }
        let internal_receipt = match installed.update(
            pending.candidate.clone(),
            &candidate_artifact,
            key.as_deref(),
            unsigned_policy(pending.allow_unsigned),
        ) {
            Err(LifecycleError::UpdateExpandsPermissions {
                diff,
                approval_receipt,
            }) if diff == pending.permission_diff => approval_receipt,
            Err(error) => return Err(error.into()),
            Ok(_) => return Err(RemoteUiPluginStoreError::Authentication),
        };
        installed.approve_update(
            &internal_receipt,
            pending.candidate.clone(),
            &candidate_artifact,
            key.as_deref(),
            unsigned_policy(pending.allow_unsigned),
        )?;
        let enabled_scope = record.enabled_scope.clone();
        let mut next =
            self.record_from_installed(&installed, pending.allow_unsigned, key.as_deref(), None)?;
        next.enabled_scope = enabled_scope;
        next.consumed_update_receipts = record.consumed_update_receipts.clone();
        remember_consumed_receipt(&mut next, approval_receipt);
        next.revision = expected_revision + 1;
        self.write_record(&next, Some(expected_revision))?;
        Ok(status(&next))
    }

    /// Commit a narrowing/same-authority candidate only after its exact staged
    /// artifact has passed UI initialization. Safe candidates have an internal
    /// receipt but never cross the human approval boundary.
    pub fn commit_safe_update(
        &self,
        plugin_id: &str,
        safe_receipt: &str,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        let _trust = self
            .trust_lock
            .lock()
            .expect("publisher trust mutex poisoned");
        let local = self.local_lock(plugin_id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(plugin_id)?;
        let record = self.read_record(plugin_id)?;
        let expected_revision = record.revision;
        let pending = record
            .pending_update
            .as_ref()
            .ok_or(RemoteUiPluginStoreError::ApprovalNotRequired)?;
        if !pending.permission_diff.is_empty() || pending.approval_receipt != safe_receipt {
            return Err(RemoteUiPluginStoreError::ApprovalReceiptMismatch);
        }
        let current_artifact = self.read_artifact(&record.artifact_checksum)?;
        let candidate_artifact = self.read_artifact(&pending.artifact_checksum)?;
        let mut installed = self.rehydrate(&record, &current_artifact)?;
        let key = self.publisher_key(&pending.candidate)?;
        if key.as_deref().map(checksum_of) != pending.publisher_key_checksum {
            return Err(RemoteUiPluginStoreError::Authentication);
        }
        installed.update(
            pending.candidate.clone(),
            &candidate_artifact,
            key.as_deref(),
            unsigned_policy(pending.allow_unsigned),
        )?;
        let enabled_scope = record.enabled_scope.clone();
        let mut next =
            self.record_from_installed(&installed, pending.allow_unsigned, key.as_deref(), None)?;
        next.enabled_scope = enabled_scope;
        next.consumed_update_receipts = record.consumed_update_receipts.clone();
        remember_consumed_receipt(&mut next, safe_receipt);
        next.revision = expected_revision + 1;
        self.write_record(&next, Some(expected_revision))?;
        Ok(status(&next))
    }

    pub fn reject_update(
        &self,
        plugin_id: &str,
        approval_receipt: &str,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        let local = self.local_lock(plugin_id);
        let _local = local.lock().expect("plugin lifecycle mutex poisoned");
        let _file = self.lock_plugin(plugin_id)?;
        let mut record = self.read_record(plugin_id)?;
        let expected_revision = record.revision;
        let pending = match record.pending_update.as_ref() {
            Some(pending) => pending,
            None if record
                .consumed_update_receipts
                .iter()
                .any(|receipt| receipt == approval_receipt) =>
            {
                return Ok(status(&record))
            }
            None => return Err(RemoteUiPluginStoreError::ApprovalNotRequired),
        };
        if pending.approval_receipt != approval_receipt {
            return Err(RemoteUiPluginStoreError::ApprovalReceiptMismatch);
        }
        record.pending_update = None;
        remember_consumed_receipt(&mut record, approval_receipt);
        record.revision = expected_revision + 1;
        self.write_record(&record, Some(expected_revision))?;
        Ok(status(&record))
    }

    pub fn status(
        &self,
        plugin_id: &str,
    ) -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
        self.read_record(plugin_id).map(|record| status(&record))
    }

    pub fn list(&self) -> Result<Vec<RemoteUiPluginStatus>, RemoteUiPluginStoreError> {
        let mut output = Vec::new();
        for path in self.record_paths()? {
            let record = self.read_record_path(&path)?;
            output.push(status(&record));
        }
        output.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(output)
    }

    /// Remove publisher trust and revoke every signed Remote UI surface from
    /// that publisher in one serialized trust transaction. Callers must stop
    /// the returned plugin ids immediately; re-verification already fails from
    /// the moment the atomically replaced trust file is durable.
    pub fn remove_trusted_publisher(
        &self,
        publisher: &str,
    ) -> Result<PublisherTrustRemoval, RemoteUiPluginStoreError> {
        let _trust = self
            .trust_lock
            .lock()
            .expect("publisher trust mutex poisoned");
        // Resolve the complete affected set while the old key is still active.
        // Any discovery/authentication error aborts before trust mutation; no
        // successful key removal can therefore lose the worker teardown set.
        let mut affected = Vec::new();
        for path in self.record_paths()? {
            let record = self.read_record_path(&path)?;
            if record.manifest.publisher == publisher && record.manifest.security.is_signed() {
                affected.push(record);
            }
        }
        let mut trusted = TrustedPublishers::load(&self.trust_store)?;
        if !trusted.remove(publisher) {
            return Err(RemoteUiPluginStoreError::Record(format!(
                "publisher `{publisher}` is not trusted"
            )));
        }
        trusted.save(&self.trust_store)?;

        let mut revoked = Vec::with_capacity(affected.len());
        let mut failures = Vec::new();
        for initial in affected {
            let plugin_id = initial.manifest.id.clone();
            let repaired = (|| -> Result<RemoteUiPluginStatus, RemoteUiPluginStoreError> {
                let local = self.local_lock(&plugin_id);
                let _local = local.lock().expect("plugin lifecycle mutex poisoned");
                let _file = self.lock_plugin(&plugin_id)?;
                let mut record = self.read_record(&plugin_id)?;
                if record.manifest.publisher != publisher || !record.manifest.security.is_signed() {
                    return Err(RemoteUiPluginStoreError::Authentication);
                }
                if record.lifecycle != StoredLifecycle::Revoked || record.pending_update.is_some() {
                    let expected = record.revision;
                    record.lifecycle = StoredLifecycle::Revoked;
                    record.enabled_scope = None;
                    record.pending_update = None;
                    record.revision = expected + 1;
                    self.write_record(&record, Some(expected))?;
                }
                Ok(status(&record))
            })();
            match repaired {
                Ok(status) => revoked.push(status),
                Err(error) => {
                    failures.push(format!("{plugin_id}: {error}"));
                    let mut effective = status(&initial);
                    effective.state = LifecycleState::Revoked;
                    effective.enabled_scope = None;
                    effective.update_approval_receipt = None;
                    effective.update_permission_diff = None;
                    revoked.push(effective);
                }
            }
        }
        revoked.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(PublisherTrustRemoval {
            plugins: revoked,
            failures,
        })
    }

    fn record_from_installed(
        &self,
        installed: &InstalledPlugin,
        allow_unsigned: bool,
        publisher_key: Option<&[u8]>,
        pending_update: Option<StoredPendingUpdate>,
    ) -> Result<StoredPlugin, RemoteUiPluginStoreError> {
        let lifecycle = match installed.state() {
            LifecycleState::InstalledDisabled => StoredLifecycle::InstalledDisabled,
            LifecycleState::SmokeTested => StoredLifecycle::SmokeTested,
            LifecycleState::Enabled => StoredLifecycle::Enabled,
            LifecycleState::Revoked => StoredLifecycle::Revoked,
            LifecycleState::UpdateBlocked => {
                return Err(RemoteUiPluginStoreError::Record(
                    "UpdateBlocked is represented by a sealed pending candidate".into(),
                ))
            }
        };
        Ok(StoredPlugin {
            record_version: RECORD_VERSION,
            revision: 1,
            manifest: installed.manifest().clone(),
            artifact_checksum: installed.content_hash().into(),
            publisher_key_checksum: publisher_key.map(checksum_of),
            allow_unsigned,
            granted: capability_spec(installed.granted()),
            granted_ui: installed.granted_ui_capabilities().clone(),
            lifecycle,
            enabled_scope: installed.enabled_scope().map(|name| EnabledScope {
                name: name.into(),
                session_id: None,
            }),
            pending_update,
            consumed_update_receipts: Vec::new(),
        })
    }

    fn rehydrate(
        &self,
        record: &StoredPlugin,
        artifact: &[u8],
    ) -> Result<InstalledPlugin, RemoteUiPluginStoreError> {
        self.validate_record(record)?;
        let key = self.publisher_key(&record.manifest)?;
        if key.as_deref().map(checksum_of) != record.publisher_key_checksum {
            return Err(RemoteUiPluginStoreError::Authentication);
        }
        let mut installed = InstalledPlugin::install_disabled(
            record.manifest.clone(),
            artifact,
            key.as_deref(),
            unsigned_policy(record.allow_unsigned),
            CapabilitySet::from_spec(&record.granted),
            record.granted_ui.clone(),
        )?;
        match record.lifecycle {
            StoredLifecycle::InstalledDisabled => {}
            StoredLifecycle::SmokeTested => installed.mark_smoke_tested()?,
            StoredLifecycle::Enabled => {
                installed.mark_smoke_tested()?;
                let scope = record.enabled_scope.as_ref().ok_or_else(|| {
                    RemoteUiPluginStoreError::Record("enabled record has no scope".into())
                })?;
                installed.enable(&scope.name)?;
            }
            StoredLifecycle::Revoked => installed.revoke(),
        }
        Ok(installed)
    }

    fn validate_record(&self, record: &StoredPlugin) -> Result<(), RemoteUiPluginStoreError> {
        if record.record_version != RECORD_VERSION {
            return Err(RemoteUiPluginStoreError::Record(format!(
                "unsupported record version {}",
                record.record_version
            )));
        }
        if record.revision == 0 {
            return Err(RemoteUiPluginStoreError::Record(
                "record revision must be greater than zero".into(),
            ));
        }
        validate_resource_limits(&record.manifest)?;
        if !manifest_has_remote_ui(&record.manifest)
            || record.artifact_checksum != record.manifest.security.checksum.trim()
            || digest_hex(&record.artifact_checksum).is_none()
        {
            return Err(RemoteUiPluginStoreError::Record(
                "record identity or artifact checksum is inconsistent".into(),
            ));
        }
        if record.pending_update.is_some() && record.lifecycle == StoredLifecycle::Revoked {
            return Err(RemoteUiPluginStoreError::Record(
                "revoked plugin cannot retain a pending update".into(),
            ));
        }
        if record.consumed_update_receipts.len() > 32
            || record
                .consumed_update_receipts
                .iter()
                .any(|receipt| receipt.is_empty() || receipt.len() > 128)
        {
            return Err(RemoteUiPluginStoreError::Record(
                "consumed update receipt history is invalid".into(),
            ));
        }
        Ok(())
    }

    fn publisher_key(
        &self,
        manifest: &PluginManifest,
    ) -> Result<Option<Vec<u8>>, RemoteUiPluginStoreError> {
        if !manifest.security.is_signed() {
            return Ok(None);
        }
        let store = TrustedPublishers::load(&self.trust_store)?;
        Ok(store.key_for(&manifest.publisher).map(|key| key.to_vec()))
    }

    fn store_package(
        &self,
        expected_checksum: &str,
        artifact: &[u8],
    ) -> Result<(), RemoteUiPluginStoreError> {
        if artifact.len() > MAX_PACKAGE_ARCHIVE_BYTES {
            return Err(RemoteUiPluginStoreError::Package(format!(
                "compressed package exceeds {} bytes",
                MAX_PACKAGE_ARCHIVE_BYTES
            )));
        }
        let actual = checksum_of(artifact);
        if actual != expected_checksum {
            return Err(RemoteUiPluginStoreError::Package(format!(
                "artifact checksum mismatch: expected {expected_checksum}, got {actual}"
            )));
        }
        let hex = digest_hex(expected_checksum).ok_or_else(|| {
            RemoteUiPluginStoreError::Package("artifact checksum is not sha256".into())
        })?;
        let artifact_path = self
            .root
            .join("artifacts")
            .join(format!("{hex}.cody-ui.tgz"));
        atomic_write_once(&artifact_path, artifact)?;
        let package_path = self.root.join("packages").join(hex);
        if package_path.exists() {
            verify_existing_package(&package_path, artifact)?;
            freeze_package_tree(&package_path)?;
            return Ok(());
        }
        let temporary = self.root.join("tmp").join(Uuid::now_v7().to_string());
        create_private_dir(&temporary)?;
        if let Err(error) = extract_package(artifact, &temporary) {
            let _ = std::fs::remove_dir_all(&temporary);
            return Err(error);
        }
        match std::fs::rename(&temporary, &package_path) {
            Ok(()) => {
                sync_directory(
                    package_path
                        .parent()
                        .expect("content-addressed package has parent"),
                )?;
                freeze_package_tree(&package_path)
            }
            Err(_source) if package_path.exists() => {
                let _ = std::fs::remove_dir_all(&temporary);
                verify_existing_package(&package_path, artifact)?;
                freeze_package_tree(&package_path)
            }
            Err(source) => {
                let _ = std::fs::remove_dir_all(&temporary);
                Err(RemoteUiPluginStoreError::Io {
                    path: package_path,
                    source,
                })
            }
        }
    }

    fn read_artifact(&self, checksum: &str) -> Result<Vec<u8>, RemoteUiPluginStoreError> {
        let hex = digest_hex(checksum)
            .ok_or_else(|| RemoteUiPluginStoreError::Record("invalid artifact checksum".into()))?;
        let path = self
            .root
            .join("artifacts")
            .join(format!("{hex}.cody-ui.tgz"));
        let bytes = std::fs::read(&path).map_err(|source| RemoteUiPluginStoreError::Io {
            path: path.clone(),
            source,
        })?;
        if checksum_of(&bytes) != checksum {
            return Err(RemoteUiPluginStoreError::Authentication);
        }
        Ok(bytes)
    }

    fn package_path(&self, checksum: &str) -> Result<PathBuf, RemoteUiPluginStoreError> {
        let hex = digest_hex(checksum)
            .ok_or_else(|| RemoteUiPluginStoreError::Record("invalid artifact checksum".into()))?;
        Ok(self.root.join("packages").join(hex))
    }

    fn write_record(
        &self,
        payload: &StoredPlugin,
        expected_revision: Option<u64>,
    ) -> Result<(), RemoteUiPluginStoreError> {
        self.validate_record(payload)?;
        match (
            expected_revision,
            self.record_path(&payload.manifest.id).exists(),
        ) {
            (None, true) => {
                return Err(RemoteUiPluginStoreError::AlreadyInstalled(
                    payload.manifest.id.clone(),
                ))
            }
            (Some(expected), true) => {
                let current = self.read_record(&payload.manifest.id)?;
                if current.revision != expected {
                    return Err(RemoteUiPluginStoreError::ConcurrentUpdate {
                        expected,
                        actual: current.revision,
                    });
                }
            }
            (Some(expected), false) => {
                return Err(RemoteUiPluginStoreError::ConcurrentUpdate {
                    expected,
                    actual: 0,
                })
            }
            (None, false) => {}
        }
        let payload_bytes = serde_json::to_vec(payload)?;
        let mac = self.mac(&payload_bytes)?;
        let sealed = serde_json::to_vec_pretty(&SealedRecord {
            payload: payload.clone(),
            mac,
        })?;
        atomic_replace(&self.record_path(&payload.manifest.id), &sealed)
    }

    fn read_record(&self, plugin_id: &str) -> Result<StoredPlugin, RemoteUiPluginStoreError> {
        let path = self.record_path(plugin_id);
        if !path.exists() {
            return Err(RemoteUiPluginStoreError::NotInstalled(plugin_id.into()));
        }
        let record = self.read_record_path(&path)?;
        if record.manifest.id != plugin_id {
            return Err(RemoteUiPluginStoreError::Authentication);
        }
        Ok(record)
    }

    fn read_record_path(&self, path: &Path) -> Result<StoredPlugin, RemoteUiPluginStoreError> {
        let file = File::open(path).map_err(|source| RemoteUiPluginStoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if file
            .metadata()
            .map_err(|source| RemoteUiPluginStoreError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .len()
            > MAX_RECORD_BYTES
        {
            return Err(RemoteUiPluginStoreError::Record(
                "record exceeds maximum size".into(),
            ));
        }
        let sealed: SealedRecord = serde_json::from_reader(file)?;
        let payload_bytes = serde_json::to_vec(&sealed.payload)?;
        self.verify_mac(&payload_bytes, &sealed.mac)?;
        self.validate_record(&sealed.payload)?;
        Ok(sealed.payload)
    }

    fn record_paths(&self) -> Result<Vec<PathBuf>, RemoteUiPluginStoreError> {
        let directory = self.root.join("records");
        let mut paths = Vec::new();
        for entry in
            std::fs::read_dir(&directory).map_err(|source| RemoteUiPluginStoreError::Io {
                path: directory.clone(),
                source,
            })?
        {
            let entry = entry.map_err(|source| RemoteUiPluginStoreError::Io {
                path: directory.clone(),
                source,
            })?;
            if entry
                .file_type()
                .map_err(|source| RemoteUiPluginStoreError::Io {
                    path: entry.path(),
                    source,
                })?
                .is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            {
                paths.push(entry.path());
            }
        }
        paths.sort();
        Ok(paths)
    }

    fn record_path(&self, plugin_id: &str) -> PathBuf {
        let digest = Sha256::digest(plugin_id.as_bytes());
        self.root
            .join("records")
            .join(format!("{}.json", hex::encode(digest)))
    }

    fn mac(&self, payload: &[u8]) -> Result<String, RemoteUiPluginStoreError> {
        let mut mac = HmacSha256::new_from_slice(&self.record_key)
            .map_err(|_| RemoteUiPluginStoreError::Authentication)?;
        mac.update(payload);
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    fn verify_mac(&self, payload: &[u8], expected: &str) -> Result<(), RemoteUiPluginStoreError> {
        let expected =
            hex::decode(expected).map_err(|_| RemoteUiPluginStoreError::Authentication)?;
        let mut mac = HmacSha256::new_from_slice(&self.record_key)
            .map_err(|_| RemoteUiPluginStoreError::Authentication)?;
        mac.update(payload);
        mac.verify_slice(&expected)
            .map_err(|_| RemoteUiPluginStoreError::Authentication)
    }

    fn local_lock(&self, plugin_id: &str) -> Arc<Mutex<()>> {
        self.local_locks
            .lock()
            .expect("plugin lock map poisoned")
            .entry(plugin_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn lock_plugin(&self, plugin_id: &str) -> Result<RecordLock, RemoteUiPluginStoreError> {
        let digest = hex::encode(Sha256::digest(plugin_id.as_bytes()));
        let path = self.root.join("locks").join(digest);
        for _ in 0..200 {
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    let owner = path.join("owner");
                    let payload = format!(
                        "{}:{}:{}",
                        std::process::id(),
                        Utc::now().timestamp(),
                        current_boot_id()
                    );
                    let authenticated = format!("{payload}\n{}\n", self.mac(payload.as_bytes())?);
                    std::fs::write(&owner, authenticated).map_err(|source| {
                        RemoteUiPluginStoreError::Io {
                            path: owner,
                            source,
                        }
                    })?;
                    return Ok(RecordLock { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if self.stale_record_lock(&path) {
                        let quarantine = self
                            .root
                            .join("locks")
                            .join(format!("stale-{}", Uuid::now_v7()));
                        if std::fs::rename(&path, &quarantine).is_ok() {
                            let _ = std::fs::remove_dir_all(quarantine);
                            continue;
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(source) => return Err(RemoteUiPluginStoreError::Io { path, source }),
            }
        }
        Err(RemoteUiPluginStoreError::Busy(plugin_id.into()))
    }

    fn stale_record_lock(&self, path: &Path) -> bool {
        let owner = match std::fs::read_to_string(path.join("owner")) {
            Ok(owner) => owner,
            Err(_) => return false,
        };
        let mut lines = owner.lines();
        let Some(payload) = lines.next() else {
            return false;
        };
        let Some(mac) = lines.next() else {
            return false;
        };
        if self.verify_mac(payload.as_bytes(), mac).is_err() {
            return false;
        }
        let mut values = payload.splitn(3, ':');
        let pid = values.next().and_then(|value| value.parse::<u32>().ok());
        let created = values.next().and_then(|value| value.parse::<i64>().ok());
        let boot = values.next().unwrap_or_default();
        if boot != current_boot_id() {
            return true;
        }
        let expired = created.is_some_and(|created| Utc::now().timestamp() - created > 30 * 60);
        #[cfg(unix)]
        let process_gone = pid.is_some_and(|pid| {
            !std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        });
        #[cfg(not(unix))]
        let process_gone = false;
        expired || process_gone
    }

    fn sweep_orphan_temps(&self) -> Result<(), RemoteUiPluginStoreError> {
        let directory = self.root.join("tmp");
        for entry in
            std::fs::read_dir(&directory).map_err(|source| RemoteUiPluginStoreError::Io {
                path: directory.clone(),
                source,
            })?
        {
            let entry = entry.map_err(|source| RemoteUiPluginStoreError::Io {
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|source| {
                RemoteUiPluginStoreError::Io {
                    path: path.clone(),
                    source,
                }
            })?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            }
            .map_err(|source| RemoteUiPluginStoreError::Io { path, source })?;
        }
        Ok(())
    }
}

async fn preflight_initial_ui(
    launch: &UiWorkerLaunch,
    worker: &mut UiWorker,
) -> Result<(), RemoteUiPluginStoreError> {
    let broker = RemoteUiBroker::default();
    let session_id = SessionId::new();
    let selection = worker
        .selection()
        .ok_or_else(|| RemoteUiPluginStoreError::Record("handshake selection missing".into()))?;
    let producer = broker
        .register_verified_producer(session_id, launch, selection)
        .map_err(|error| RemoteUiPluginStoreError::Record(error.to_string()))?;
    let expected = launch
        .verified_contributions()
        .values()
        .filter(|contribution| contribution.targets.contains(&launch.target()))
        .map(|contribution| contribution.id.clone())
        .collect::<BTreeSet<_>>();
    let mut documents = HashSet::new();
    let mut contributions = std::collections::BTreeMap::<String, String>::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let complete = if expected.is_empty() {
            !documents.is_empty()
        } else {
            expected.iter().all(|id| {
                contributions
                    .get(id)
                    .is_some_and(|document| documents.contains(document))
            })
        };
        if complete {
            let _ = broker.dispose_producer(session_id, &producer);
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(RemoteUiPluginStoreError::Record(
                "UI smoke test timed out before every declared surface initialized".into(),
            ));
        }
        let signal = tokio::time::timeout(remaining, worker.next_signal())
            .await
            .map_err(|_| {
                RemoteUiPluginStoreError::Record(
                    "UI smoke test timed out before every declared surface initialized".into(),
                )
            })??;
        let UiWorkerSignal::Message(message) = signal else {
            continue;
        };
        let message = *message;
        let snapshot_document = message
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.document.document_id.to_string());
        let advertised = message
            .contributions
            .iter()
            .map(|contribution| {
                (
                    contribution.id.to_string(),
                    contribution.document_id.to_string(),
                )
            })
            .collect::<Vec<_>>();
        broker
            .handle_producer(session_id, &producer, message)
            .map_err(|error| RemoteUiPluginStoreError::Record(error.to_string()))?;
        if let Some(document) = snapshot_document {
            documents.insert(document);
        }
        for (id, document) in advertised {
            contributions.insert(id, document);
        }
    }
}

struct RecordLock {
    path: PathBuf,
}

impl Drop for RecordLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path.join("owner"));
        let _ = std::fs::remove_dir(&self.path);
    }
}

fn current_boot_id() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(value) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        return value.trim().to_owned();
    }
    #[cfg(target_os = "macos")]
    if let Ok(output) = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.boottime"])
        .output()
    {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_owned();
        }
    }
    "unknown-boot".into()
}

impl VerifiedUiLaunchSource for RemoteUiPluginStore {
    fn launches_for(&self, session_id: SessionId) -> Vec<UiWorkerLaunch> {
        let mut launches = Vec::new();
        let records = match self.record_paths() {
            Ok(records) => records,
            Err(error) => {
                tracing::error!(%error, "cannot enumerate Remote UI plugin records");
                return launches;
            }
        };
        for path in records {
            let result = (|| -> Result<Vec<UiWorkerLaunch>, RemoteUiPluginStoreError> {
                let record = self.read_record_path(&path)?;
                if record.lifecycle != StoredLifecycle::Enabled {
                    return Ok(Vec::new());
                }
                if record
                    .enabled_scope
                    .as_ref()
                    .and_then(|scope| scope.session_id)
                    .is_some_and(|bound| bound != session_id)
                {
                    return Ok(Vec::new());
                }
                let artifact = self.read_artifact(&record.artifact_checksum)?;
                let installed = self.rehydrate(&record, &artifact)?;
                let package = self.package_path(&record.artifact_checksum)?;
                launch_targets(installed.manifest())
                    .into_iter()
                    .map(|target| {
                        UiWorkerLaunch::from_installed_with_runtime(
                            &installed,
                            &artifact,
                            &package,
                            target,
                            self.runtime.clone(),
                            UiWorkerLaunchPurpose::Active,
                        )
                        .map_err(Into::into)
                    })
                    .collect()
            })();
            match result {
                Ok(mut verified) => launches.append(&mut verified),
                Err(error) => {
                    tracing::error!(path = %path.display(), %error, "Remote UI plugin failed closed during launch re-verification")
                }
            }
        }
        launches
    }
}

/// Construct the enforcing supervisor and a trusted, self-contained Node
/// runtime. Release builds never discover a system/Homebrew Node through PATH:
/// they use an application-bundled tree, or an explicit operator-pinned
/// executable *and* root. That keeps the sandbox read mount narrow and prevents
/// package execution from inheriting a mutable system prefix.
pub fn system_remote_ui_runtime(
) -> Result<(UiWorkerRuntime, Arc<UiWorkerSupervisor>), RemoteUiPluginStoreError> {
    let configured_node = std::env::var_os("CODYPENDENT_UI_NODE");
    let configured_root = std::env::var_os("CODYPENDENT_UI_NODE_RUNTIME_ROOT");
    let runtime = match (configured_node, configured_root) {
        (Some(executable), Some(root)) => {
            UiWorkerRuntime::new(PathBuf::from(executable), PathBuf::from(root))?
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(RemoteUiPluginStoreError::Record(
                "CODYPENDENT_UI_NODE and CODYPENDENT_UI_NODE_RUNTIME_ROOT must be set together"
                    .into(),
            ));
        }
        (None, None) => match bundled_ui_runtime()? {
            Some(runtime) => runtime,
            None if cfg!(debug_assertions) => {
                let executable = find_in_path("node").ok_or_else(|| {
                    RemoteUiPluginStoreError::Record(
                        "Node.js >=22.13 was not found for the debug Remote UI runtime".into(),
                    )
                })?;
                let executable = std::fs::canonicalize(&executable).map_err(|source| {
                    RemoteUiPluginStoreError::Io {
                        path: executable.clone(),
                        source,
                    }
                })?;
                let root = executable.parent().and_then(Path::parent).ok_or_else(|| {
                    RemoteUiPluginStoreError::Record(
                        "debug Node executable has no self-contained runtime root".into(),
                    )
                })?;
                UiWorkerRuntime::new(&executable, root)?
            }
            None => {
                return Err(RemoteUiPluginStoreError::Record(
                    "bundled Remote UI Node runtime is unavailable; release builds do not use PATH"
                        .into(),
                ));
            }
        },
    };
    let supervisor = Arc::new(UiWorkerSupervisor::system(UiWorkerConfig::default())?);
    Ok((runtime, supervisor))
}

fn bundled_ui_runtime() -> Result<Option<UiWorkerRuntime>, RemoteUiPluginStoreError> {
    let current = std::env::current_exe().map_err(|source| RemoteUiPluginStoreError::Io {
        path: PathBuf::from("current executable"),
        source,
    })?;
    let Some(bin) = current.parent() else {
        return Ok(None);
    };
    let mut candidates = vec![bin.join("node-runtime")];
    if let Some(prefix) = bin.parent() {
        candidates.push(prefix.join("lib/codypendent/node-runtime"));
    }
    for root in candidates {
        let executable = root.join("bin/node");
        if !executable.is_file() {
            continue;
        }
        let root = std::fs::canonicalize(&root).map_err(|source| RemoteUiPluginStoreError::Io {
            path: root.clone(),
            source,
        })?;
        let executable =
            std::fs::canonicalize(&executable).map_err(|source| RemoteUiPluginStoreError::Io {
                path: executable.clone(),
                source,
            })?;
        if !executable.starts_with(&root) || !trusted_runtime_permissions(&root, &executable) {
            return Err(RemoteUiPluginStoreError::Authentication);
        }
        let verification_started = std::time::Instant::now();
        verify_bundled_runtime_root(&root)?;
        tracing::info!(
            elapsed_ms = verification_started.elapsed().as_millis(),
            "bundled Node runtime integrity verified"
        );
        return UiWorkerRuntime::new(executable, root)
            .map(Some)
            .map_err(Into::into);
    }
    Ok(None)
}

fn trusted_runtime_permissions(root: &Path, executable: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let Ok(host) = std::env::current_exe().and_then(std::fs::metadata) else {
            return false;
        };
        for path in [root, executable] {
            let Ok(metadata) = std::fs::metadata(path) else {
                return false;
            };
            if metadata.uid() != host.uid() || metadata.permissions().mode() & 0o022 != 0 {
                return false;
            }
        }
    }
    true
}

pub fn verify_bundled_runtime_root(root: &Path) -> Result<(), RemoteUiPluginStoreError> {
    let seal_path = root.join(RUNTIME_SEAL_FILE);
    let seal_file = File::open(&seal_path).map_err(|source| RemoteUiPluginStoreError::Io {
        path: seal_path.clone(),
        source,
    })?;
    let mut encoded = Vec::new();
    seal_file
        .take(MAX_RUNTIME_SEAL_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|source| RemoteUiPluginStoreError::Io {
            path: seal_path.clone(),
            source,
        })?;
    if encoded.len() as u64 > MAX_RUNTIME_SEAL_BYTES {
        return Err(RemoteUiPluginStoreError::Record(
            "bundled Node runtime seal exceeds the host byte limit".into(),
        ));
    }
    let mut expected: Vec<RuntimeSealEntry> = serde_json::from_slice(&encoded)?;
    if expected.len() > MAX_RUNTIME_ENTRIES {
        return Err(RemoteUiPluginStoreError::Record(
            "bundled Node runtime seal exceeds the host entry limit".into(),
        ));
    }
    expected.sort();
    if expected.windows(2).any(|pair| pair[0].path == pair[1].path) {
        return Err(RemoteUiPluginStoreError::Authentication);
    }
    for entry in &expected {
        let relative = normalized_path(Path::new(&entry.path))?;
        if relative.is_absolute()
            || relative.as_os_str().is_empty()
            || relative == Path::new(RUNTIME_SEAL_FILE)
            || !matches!(entry.kind.as_str(), "file" | "link")
        {
            return Err(RemoteUiPluginStoreError::Authentication);
        }
    }

    let canonical_root =
        std::fs::canonicalize(root).map_err(|source| RemoteUiPluginStoreError::Io {
            path: root.to_path_buf(),
            source,
        })?;
    let mut actual = Vec::new();
    let mut total_bytes = 0_u64;
    collect_runtime_seal(
        &canonical_root,
        &canonical_root,
        &mut actual,
        &mut total_bytes,
    )?;
    actual.sort();
    if actual != expected {
        return Err(RemoteUiPluginStoreError::Authentication);
    }
    Ok(())
}

fn collect_runtime_seal(
    root: &Path,
    directory: &Path,
    output: &mut Vec<RuntimeSealEntry>,
    total_bytes: &mut u64,
) -> Result<(), RemoteUiPluginStoreError> {
    let entries = std::fs::read_dir(directory).map_err(|source| RemoteUiPluginStoreError::Io {
        path: directory.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| RemoteUiPluginStoreError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| RemoteUiPluginStoreError::Authentication)?;
        if relative == Path::new(RUNTIME_SEAL_FILE) {
            continue;
        }
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| RemoteUiPluginStoreError::Io {
                path: path.clone(),
                source,
            })?;
        if metadata.is_dir() {
            collect_runtime_seal(root, &path, output, total_bytes)?;
            continue;
        }
        if output.len() >= MAX_RUNTIME_ENTRIES {
            return Err(RemoteUiPluginStoreError::Record(
                "bundled Node runtime exceeds the host entry limit".into(),
            ));
        }
        let path_text = relative.to_string_lossy().replace('\\', "/");
        if metadata.file_type().is_symlink() {
            let resolved =
                std::fs::canonicalize(&path).map_err(|source| RemoteUiPluginStoreError::Io {
                    path: path.clone(),
                    source,
                })?;
            if !resolved.starts_with(root) {
                return Err(RemoteUiPluginStoreError::Authentication);
            }
            let target =
                std::fs::read_link(&path).map_err(|source| RemoteUiPluginStoreError::Io {
                    path: path.clone(),
                    source,
                })?;
            output.push(RuntimeSealEntry {
                kind: "link".into(),
                path: path_text,
                digest: target.to_string_lossy().into_owned(),
            });
        } else if metadata.is_file() {
            *total_bytes = total_bytes.saturating_add(metadata.len());
            if *total_bytes > MAX_RUNTIME_BYTES {
                return Err(RemoteUiPluginStoreError::Record(
                    "bundled Node runtime exceeds the host byte limit".into(),
                ));
            }
            let mut file = File::open(&path).map_err(|source| RemoteUiPluginStoreError::Io {
                path: path.clone(),
                source,
            })?;
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count =
                    file.read(&mut buffer)
                        .map_err(|source| RemoteUiPluginStoreError::Io {
                            path: path.clone(),
                            source,
                        })?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            output.push(RuntimeSealEntry {
                kind: "file".into(),
                path: path_text,
                digest: hex::encode(digest.finalize()),
            });
        } else {
            return Err(RemoteUiPluginStoreError::Authentication);
        }
    }
    Ok(())
}

fn manifest_has_remote_ui(manifest: &PluginManifest) -> bool {
    manifest.ui.is_some()
        && matches!(
            manifest.kind,
            PluginKind::UiComponent | PluginKind::NativeProcess
        )
}

fn status(record: &StoredPlugin) -> RemoteUiPluginStatus {
    let state = if record.pending_update.is_some() {
        LifecycleState::UpdateBlocked
    } else {
        match record.lifecycle {
            StoredLifecycle::InstalledDisabled => LifecycleState::InstalledDisabled,
            StoredLifecycle::SmokeTested => LifecycleState::SmokeTested,
            StoredLifecycle::Enabled => LifecycleState::Enabled,
            StoredLifecycle::Revoked => LifecycleState::Revoked,
        }
    };
    RemoteUiPluginStatus {
        id: record.manifest.id.clone(),
        version: record.manifest.version.clone(),
        state,
        enabled_scope: record.enabled_scope.as_ref().map(|scope| {
            scope.session_id.map_or_else(
                || scope.name.clone(),
                |session_id| format!("{}:{session_id}", scope.name),
            )
        }),
        update_approval_receipt: record
            .pending_update
            .as_ref()
            .map(|pending| pending.approval_receipt.clone()),
        update_permission_diff: record
            .pending_update
            .as_ref()
            .map(|pending| pending.permission_diff.clone()),
    }
}

fn remember_consumed_receipt(record: &mut StoredPlugin, receipt: &str) {
    if !record
        .consumed_update_receipts
        .iter()
        .any(|value| value == receipt)
    {
        record.consumed_update_receipts.push(receipt.to_owned());
    }
    if record.consumed_update_receipts.len() > 32 {
        let extra = record.consumed_update_receipts.len() - 32;
        record.consumed_update_receipts.drain(..extra);
    }
}

fn launch_targets(manifest: &PluginManifest) -> Vec<UiTarget> {
    let Some(ui) = &manifest.ui else {
        return Vec::new();
    };
    let mut targets = ui
        .contributions
        .iter()
        .flat_map(|contribution| contribution.targets.iter().copied())
        .collect::<BTreeSet<_>>();
    // A concrete override is executable surface even if a package currently
    // declares no mounted contribution for it (for example a setup flow that
    // registers after handshake).
    if ui.entrypoints.terminal.is_some() {
        targets.insert(UiTarget::Terminal);
    }
    if ui.entrypoints.web.is_some() {
        targets.insert(UiTarget::Web);
    }
    if targets.is_empty() && ui.entrypoints.shared.is_some() {
        targets.insert(UiTarget::Shared);
    }
    targets.into_iter().collect()
}

fn capability_spec(granted: &CapabilitySet) -> CapabilitiesSpec {
    let mut output = CapabilitiesSpec::default();
    for capability in granted.iter() {
        match capability {
            Capability::FilesystemRead(value) => output.filesystem_read.push(value.clone()),
            Capability::FilesystemWrite(value) => output.filesystem_write.push(value.clone()),
            Capability::Network(value) => output.network.push(value.clone()),
            Capability::Secret(value) => output.secrets.push(value.clone()),
            Capability::Subprocess => output.subprocess = true,
        }
    }
    output
}

fn unsigned_policy(allow: bool) -> UnsignedPolicy {
    if allow {
        UnsignedPolicy::Allow
    } else {
        UnsignedPolicy::Deny
    }
}

fn digest_hex(checksum: &str) -> Option<&str> {
    let digest = checksum.strip_prefix("sha256:")?;
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(digest)
}

fn normalized_path(path: &Path) -> Result<PathBuf, RemoteUiPluginStoreError> {
    codypendent_sandbox::package::normalized_path(path).map_err(|e| match e {
        codypendent_sandbox::PackageError::Io { path, source } => {
            RemoteUiPluginStoreError::Io { path, source }
        }
        codypendent_sandbox::PackageError::Authentication => {
            RemoteUiPluginStoreError::Authentication
        }
        other => RemoteUiPluginStoreError::Package(other.to_string()),
    })
}

fn extract_package(artifact: &[u8], root: &Path) -> Result<(), RemoteUiPluginStoreError> {
    codypendent_sandbox::package::extract_package(artifact, root).map_err(|e| match e {
        codypendent_sandbox::PackageError::Io { path, source } => {
            RemoteUiPluginStoreError::Io { path, source }
        }
        codypendent_sandbox::PackageError::Authentication => {
            RemoteUiPluginStoreError::Authentication
        }
        other => RemoteUiPluginStoreError::Package(other.to_string()),
    })
}

fn verify_existing_package(root: &Path, artifact: &[u8]) -> Result<(), RemoteUiPluginStoreError> {
    codypendent_sandbox::package::verify_existing_package(root, artifact).map_err(|e| match e {
        codypendent_sandbox::PackageError::Io { path, source } => {
            RemoteUiPluginStoreError::Io { path, source }
        }
        codypendent_sandbox::PackageError::Authentication => {
            RemoteUiPluginStoreError::Authentication
        }
        other => RemoteUiPluginStoreError::Package(other.to_string()),
    })
}

fn freeze_package_tree(root: &Path) -> Result<(), RemoteUiPluginStoreError> {
    codypendent_sandbox::package::freeze_package_tree(root).map_err(|e| match e {
        codypendent_sandbox::PackageError::Io { path, source } => {
            RemoteUiPluginStoreError::Io { path, source }
        }
        codypendent_sandbox::PackageError::Authentication => {
            RemoteUiPluginStoreError::Authentication
        }
        other => RemoteUiPluginStoreError::Package(other.to_string()),
    })
}

fn create_private_dir(path: &Path) -> Result<(), RemoteUiPluginStoreError> {
    codypendent_sandbox::package::create_private_dir(path).map_err(|e| match e {
        codypendent_sandbox::PackageError::Io { path, source } => {
            RemoteUiPluginStoreError::Io { path, source }
        }
        codypendent_sandbox::PackageError::Authentication => {
            RemoteUiPluginStoreError::Authentication
        }
        other => RemoteUiPluginStoreError::Package(other.to_string()),
    })
}

fn atomic_write_once(path: &Path, bytes: &[u8]) -> Result<(), RemoteUiPluginStoreError> {
    codypendent_sandbox::package::atomic_write_once(path, bytes).map_err(|e| match e {
        codypendent_sandbox::PackageError::Io { path, source } => {
            RemoteUiPluginStoreError::Io { path, source }
        }
        codypendent_sandbox::PackageError::Authentication => {
            RemoteUiPluginStoreError::Authentication
        }
        other => RemoteUiPluginStoreError::Package(other.to_string()),
    })
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), RemoteUiPluginStoreError> {
    codypendent_sandbox::package::atomic_replace(path, bytes).map_err(|e| match e {
        codypendent_sandbox::PackageError::Io { path, source } => {
            RemoteUiPluginStoreError::Io { path, source }
        }
        codypendent_sandbox::PackageError::Authentication => {
            RemoteUiPluginStoreError::Authentication
        }
        other => RemoteUiPluginStoreError::Package(other.to_string()),
    })
}

fn sync_directory(path: &Path) -> Result<(), RemoteUiPluginStoreError> {
    codypendent_sandbox::package::sync_directory(path).map_err(|e| match e {
        codypendent_sandbox::PackageError::Io { path, source } => {
            RemoteUiPluginStoreError::Io { path, source }
        }
        codypendent_sandbox::PackageError::Authentication => {
            RemoteUiPluginStoreError::Authentication
        }
        other => RemoteUiPluginStoreError::Package(other.to_string()),
    })
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn validate_resource_limits(manifest: &PluginManifest) -> Result<(), RemoteUiPluginStoreError> {
    for (field, value, maximum) in [
        (
            "memory_mb",
            manifest.resources.memory_mb,
            MAX_PLUGIN_MEMORY_MB,
        ),
        (
            "cpu_seconds",
            manifest.resources.cpu_seconds,
            MAX_PLUGIN_CPU_SECONDS,
        ),
        (
            "wall_seconds",
            manifest.resources.wall_seconds,
            MAX_PLUGIN_WALL_SECONDS,
        ),
        (
            "maximum_output_mb",
            manifest.resources.maximum_output_mb,
            MAX_PLUGIN_OUTPUT_MB,
        ),
    ] {
        if value > maximum {
            return Err(RemoteUiPluginStoreError::ResourceLimit { field, maximum });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_ui::{RemoteUiBroker, UiBrokerTarget};
    use crate::remote_ui_workers::{RemoteUiWorkerService, UiWorkerRequest};
    use base64::Engine as _;
    use codypendent_protocol::{
        ClientId, ClientRole, UiActionResult, UiClientKind, UiEventId, UiProjectionUpdate,
        UiWireMessage,
    };
    use codypendent_sandbox::{parse_manifest, signing_digest};
    use ed25519_dalek::{Signer as _, SigningKey};
    use flate2::{write::GzEncoder, Compression};
    use tempfile::tempdir;

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder.append_data(&mut header, *path, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn manifest(artifact: &[u8]) -> PluginManifest {
        let text = format!(
            r#"schema_version = 1
id = "test-ui"
name = "Test UI"
version = "1.0.0"
kind = "ui-component"
publisher = "test"
scopes = ["user", "session"]

[security]
checksum = "{}"
signature = ""

[ui]
schema_version = 1
requested_capabilities = ["context-read"]

[ui.compatibility]
protocol = "^1.0.0"
sdk = "^1.0.0"

[ui.entrypoints]
shared = "worker.mjs"
"#,
            checksum_of(artifact)
        );
        parse_manifest(&text).unwrap()
    }

    fn manifest_with_targets(entrypoints: &str, contributions: &str) -> PluginManifest {
        let text = format!(
            r#"schema_version = 1
id = "target-ui"
name = "Target UI"
version = "1.0.0"
kind = "ui-component"
publisher = "test"
scopes = ["user", "session"]

[security]
checksum = "{}"
signature = ""

[ui]
schema_version = 1
requested_capabilities = ["context-read"]

[ui.compatibility]
protocol = "^1.0.0"
sdk = "^1.0.0"

[ui.entrypoints]
{entrypoints}

{contributions}
"#,
            checksum_of(b"target")
        );
        parse_manifest(&text).unwrap()
    }

    fn runtime() -> Option<UiWorkerRuntime> {
        let executable = find_in_path("node")?;
        let executable = std::fs::canonicalize(executable).ok()?;
        let root = executable.parent()?.parent()?.to_path_buf();
        UiWorkerRuntime::new(executable, root).ok()
    }

    #[test]
    fn durable_record_rehydrates_only_after_mac_and_artifact_verification() {
        let Some(runtime) = runtime() else { return };
        let directory = tempdir().unwrap();
        let worker = archive(&[("worker.mjs", b"export {};" as &[u8])]);
        let manifest = manifest(&worker);
        let store =
            RemoteUiPluginStore::open(directory.path(), directory.path(), runtime, vec![7; 32])
                .unwrap();
        let granted = CapabilitySet::from_spec(&manifest.capabilities);
        let granted_ui = manifest
            .ui
            .as_ref()
            .unwrap()
            .requested_capabilities
            .iter()
            .copied()
            .collect();
        let installed = store
            .install_disabled(manifest, &worker, true, granted, granted_ui)
            .unwrap();
        assert_eq!(installed.state, LifecycleState::InstalledDisabled);
        let reopened = RemoteUiPluginStore::open(
            directory.path(),
            directory.path(),
            store.runtime.clone(),
            vec![7; 32],
        )
        .unwrap();
        assert_eq!(reopened.status("test-ui").unwrap(), installed);

        let record = reopened.record_path("test-ui");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        value["payload"]["lifecycle"] = serde_json::json!("enabled");
        std::fs::write(&record, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            reopened.status("test-ui"),
            Err(RemoteUiPluginStoreError::Authentication)
        ));
    }

    #[test]
    fn enabled_session_scope_filters_verified_launches() {
        let Some(runtime) = runtime() else { return };
        let directory = tempdir().unwrap();
        let worker = archive(&[("worker.mjs", b"export {};" as &[u8])]);
        let manifest = manifest(&worker);
        let store =
            RemoteUiPluginStore::open(directory.path(), directory.path(), runtime, vec![9; 32])
                .unwrap();
        let granted = CapabilitySet::from_spec(&manifest.capabilities);
        let granted_ui = manifest
            .ui
            .as_ref()
            .unwrap()
            .requested_capabilities
            .iter()
            .copied()
            .collect();
        store
            .install_disabled(manifest, &worker, true, granted, granted_ui)
            .unwrap();
        let mut record = store.read_record("test-ui").unwrap();
        record.lifecycle = StoredLifecycle::SmokeTested;
        let expected = record.revision;
        record.revision += 1;
        store.write_record(&record, Some(expected)).unwrap();
        let session = SessionId::new();
        store.enable("test-ui", "session", Some(session)).unwrap();
        assert_eq!(store.launches_for(session).len(), 1);
        assert!(store.launches_for(SessionId::new()).is_empty());
    }

    #[test]
    fn package_paths_reject_traversal_and_excessive_depth() {
        assert!(normalized_path(Path::new("../escape.mjs")).is_err());
        let deep = std::iter::repeat_n(
            "directory",
            codypendent_sandbox::package::MAX_ARCHIVE_PATH_DEPTH + 1,
        )
        .collect::<Vec<_>>()
        .join("/");
        assert!(normalized_path(Path::new(&deep)).is_err());
    }

    #[test]
    fn stale_lifecycle_record_cannot_overwrite_a_newer_revision() {
        let Some(runtime) = runtime() else { return };
        let directory = tempdir().unwrap();
        let worker = archive(&[("worker.mjs", b"export {};" as &[u8])]);
        let manifest = manifest(&worker);
        let store =
            RemoteUiPluginStore::open(directory.path(), directory.path(), runtime, vec![31; 32])
                .unwrap();
        let granted = CapabilitySet::from_spec(&manifest.capabilities);
        let granted_ui = manifest
            .ui
            .as_ref()
            .unwrap()
            .requested_capabilities
            .iter()
            .copied()
            .collect();
        store
            .install_disabled(manifest, &worker, true, granted, granted_ui)
            .unwrap();
        let mut first = store.read_record("test-ui").unwrap();
        let mut stale = first.clone();
        let base = first.revision;
        first.lifecycle = StoredLifecycle::Revoked;
        first.revision += 1;
        store.write_record(&first, Some(base)).unwrap();
        stale.lifecycle = StoredLifecycle::Enabled;
        stale.revision += 1;
        assert!(matches!(
            store.write_record(&stale, Some(base)),
            Err(RemoteUiPluginStoreError::ConcurrentUpdate { .. })
        ));
        assert_eq!(
            store.read_record("test-ui").unwrap().lifecycle,
            StoredLifecycle::Revoked
        );
    }

    #[cfg(unix)]
    #[test]
    fn trust_removal_preserves_affected_ids_when_record_repair_fails() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let Some(runtime) = runtime() else { return };
        let directory = tempdir().unwrap();
        // The 0o500 on `records` below is what makes the repair fail, and mode
        // bits do not bind uid 0 (CAP_DAC_OVERRIDE): as root the repair
        // succeeds and there is no failure to preserve. Skip in root
        // containers rather than assert one anyway.
        if std::fs::metadata(directory.path())
            .expect("stat tempdir")
            .uid()
            == 0
        {
            return;
        }
        let worker = archive(&[("worker.mjs", b"export {};" as &[u8])]);
        let signing = SigningKey::from_bytes(&[61_u8; 32]);
        let mut trusted = TrustedPublishers::new();
        trusted
            .add(
                "test",
                &base64::engine::general_purpose::STANDARD
                    .encode(signing.verifying_key().to_bytes()),
            )
            .unwrap();
        trusted
            .save(&directory.path().join("trusted_publishers.toml"))
            .unwrap();
        let mut manifest = manifest(&worker);
        manifest.security.signature = base64::engine::general_purpose::STANDARD
            .encode(signing.sign(&signing_digest(&manifest)).to_bytes());
        let store =
            RemoteUiPluginStore::open(directory.path(), directory.path(), runtime, vec![63; 32])
                .unwrap();
        store
            .install_disabled(
                manifest,
                &worker,
                false,
                CapabilitySet::default(),
                BTreeSet::from([UiCapability::ContextRead]),
            )
            .unwrap();

        let records = store.root.join("records");
        std::fs::set_permissions(&records, std::fs::Permissions::from_mode(0o500)).unwrap();
        let removal = store.remove_trusted_publisher("test").unwrap();
        std::fs::set_permissions(&records, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(removal.plugins.len(), 1);
        assert_eq!(removal.plugins[0].id, "test-ui");
        assert_eq!(removal.plugins[0].state, LifecycleState::Revoked);
        assert_eq!(removal.failures.len(), 1);
        assert!(
            TrustedPublishers::load(&directory.path().join("trusted_publishers.toml"))
                .unwrap()
                .key_for("test")
                .is_none()
        );
    }

    #[test]
    fn shared_code_is_selected_for_a_web_only_contribution() {
        let manifest = manifest_with_targets(
            r#"shared = "worker.mjs""#,
            r#"[[ui.contributions]]
id = "target.web"
point = "panel"
renderer = "target.Web"
targets = ["web"]
fallback_renderer = "target.Terminal"

[[ui.contributions]]
id = "target.terminal"
point = "panel"
renderer = "target.Terminal"
targets = ["terminal"]"#,
        );
        assert_eq!(
            launch_targets(&manifest),
            vec![UiTarget::Terminal, UiTarget::Web]
        );
    }

    #[test]
    fn concrete_web_override_keeps_shared_code_as_other_target_fallback() {
        let manifest = manifest_with_targets(
            r#"shared = "worker.mjs"
web = "web.mjs""#,
            r#"[[ui.contributions]]
id = "target.terminal"
point = "panel"
renderer = "target.Terminal"
targets = ["terminal"]"#,
        );
        let targets = launch_targets(&manifest);
        assert!(targets.contains(&UiTarget::Terminal));
        assert!(targets.contains(&UiTarget::Web));
        assert!(!targets.contains(&UiTarget::Shared));
    }

    #[tokio::test]
    async fn terminal_and_web_attach_launch_only_their_verified_real_contributions() {
        let (runtime, supervisor) = match system_remote_ui_runtime() {
            Ok(value) => value,
            Err(_) => return,
        };
        let source = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/remote_ui_fallback_worker.mjs"),
        )
        .unwrap();
        let artifact = archive(&[("worker.mjs", source.as_slice())]);
        let manifest = parse_manifest(&format!(
            r#"schema_version = 1
id = "fallback-ui"
name = "Fallback UI"
version = "1.0.0"
kind = "ui-component"
publisher = "test"
scopes = ["session"]

[security]
checksum = "{}"
signature = ""

[ui]
schema_version = 1
requested_capabilities = []

[ui.compatibility]
protocol = "^1.0.0"
sdk = "^1.0.0"

[ui.entrypoints]
shared = "worker.mjs"

[[ui.contributions]]
id = "fallback-ui.web-panel"
point = "panel"
renderer = "fallback.WebPanel"
targets = ["web"]
fallback_renderer = "fallback.TerminalPanel"

[[ui.contributions]]
id = "fallback-ui.terminal-panel"
point = "panel"
renderer = "fallback.TerminalPanel"
targets = ["terminal"]
"#,
            checksum_of(&artifact)
        ))
        .unwrap();
        let directory = tempdir().unwrap();
        let store = Arc::new(
            RemoteUiPluginStore::open(directory.path(), directory.path(), runtime, vec![53; 32])
                .unwrap(),
        );
        store
            .install_disabled(
                manifest,
                &artifact,
                true,
                CapabilitySet::default(),
                BTreeSet::new(),
            )
            .unwrap();
        let mut record = store.read_record("fallback-ui").unwrap();
        let expected = record.revision;
        record.lifecycle = StoredLifecycle::SmokeTested;
        record.revision += 1;
        store.write_record(&record, Some(expected)).unwrap();
        let session_id = SessionId::new();
        store
            .enable("fallback-ui", "session", Some(session_id))
            .unwrap();

        let broker = RemoteUiBroker::default();
        let client_id = ClientId::new();
        let mut renderer = broker.subscribe_renderer(session_id, client_id).unwrap();
        let mut capabilities = broker.producer_offer();
        capabilities.client = UiClientKind::from("terminal");
        broker
            .handle_renderer(
                session_id,
                client_id,
                ClientRole::Controller,
                serde_json::from_value(serde_json::json!({
                    "type": "capabilities",
                    "messageId": "fallback-terminal-capabilities",
                    "capabilities": capabilities,
                }))
                .unwrap(),
            )
            .unwrap();
        let service = RemoteUiWorkerService::new(supervisor, store);
        let (requests, _request_rx) = tokio::sync::mpsc::channel(4);
        assert_eq!(
            service.ensure_session_target(
                session_id,
                UiTarget::Terminal,
                broker.clone(),
                requests.clone(),
            ),
            1
        );
        assert_eq!(service.active_count(session_id), 1);
        let (saw_terminal, saw_snapshot) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let mut contribution = false;
                let mut snapshot = false;
                while !contribution || !snapshot {
                    let frame = renderer.receiver.recv().await.unwrap();
                    contribution |= frame.message.contributions.iter().any(|registration| {
                        registration.id.as_str() == "fallback-ui.terminal-panel"
                    });
                    snapshot |= frame.message.snapshot.is_some();
                }
                (contribution, snapshot)
            })
            .await
            .expect("terminal renderer receives the declared fallback contribution");
        assert!(saw_terminal && saw_snapshot);

        let web_client = ClientId::new();
        let mut web_renderer = broker.subscribe_renderer(session_id, web_client).unwrap();
        let mut web_capabilities = broker.producer_offer();
        web_capabilities.client = UiClientKind::from("web");
        broker
            .handle_renderer(
                session_id,
                web_client,
                ClientRole::Controller,
                serde_json::from_value(serde_json::json!({
                    "type": "capabilities",
                    "messageId": "fallback-web-capabilities",
                    "capabilities": web_capabilities,
                }))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            service.ensure_session_target(session_id, UiTarget::Web, broker.clone(), requests,),
            1
        );
        assert_eq!(service.active_count(session_id), 2);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let frame = web_renderer.receiver.recv().await.unwrap();
                if frame
                    .message
                    .contributions
                    .iter()
                    .any(|registration| registration.id.as_str() == "fallback-ui.web-panel")
                {
                    break;
                }
            }
        })
        .await
        .expect("web renderer receives the declared web contribution");
        assert_eq!(service.stop_session_target(session_id, UiTarget::Web), 1);
        assert_eq!(service.active_count(session_id), 1);
        assert_eq!(service.stop_session(session_id), 1);
    }

    /// Full enforcing-process lifecycle: verified archive installation and real
    /// smoke test, scoped enablement, renderer attach, worker contribution and
    /// snapshot, mediated projection, gesture-bound action, result delivery,
    /// and detach-driven dispose. The test skips only when the platform's
    /// enforcing sandbox/resource launcher or Node 22 runtime is unavailable.
    #[tokio::test]
    async fn real_process_install_enable_attach_mediate_and_teardown() {
        let (runtime, supervisor) = match system_remote_ui_runtime() {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("skipping enforcing Remote UI lifecycle fixture: {error}");
                return;
            }
        };
        let source = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/remote_ui_lifecycle_worker.mjs"),
        )
        .unwrap();
        let artifact = archive(&[
            ("worker.mjs", source.as_slice()),
            ("bin/native", b"native fixture" as &[u8]),
        ]);
        let manifest_text = format!(
            r#"schema_version = 1
id = "lifecycle-ui"
name = "Lifecycle UI"
version = "1.0.0"
kind = "native-process"
publisher = "test"
scopes = ["user", "session"]

[runtime]
command = "bin/native"
protocol = "mcp-stdio"
working_directory = "isolated"

[capabilities]
network = ["api.example.invalid:443"]

[security]
checksum = "{}"
signature = ""

[ui]
schema_version = 1
requested_capabilities = ["artifact-read", "context-read", "workflow-read", "command-invoke"]

[ui.compatibility]
protocol = "^1.0.0"
sdk = "^1.0.0"

[ui.entrypoints]
terminal = "worker.mjs"

[[ui.contributions]]
id = "lifecycle-ui.panel"
point = "panel"
renderer = "lifecycle.Panel"
targets = ["terminal"]

[resources]
memory_mb = 128
cpu_seconds = 30
wall_seconds = 60
maximum_output_mb = 8
"#,
            checksum_of(&artifact)
        );
        let mut manifest = parse_manifest(&manifest_text).unwrap();
        let directory = tempdir().unwrap();
        let signing = SigningKey::from_bytes(&[47_u8; 32]);
        let mut trusted = TrustedPublishers::new();
        trusted
            .add(
                "test",
                &base64::engine::general_purpose::STANDARD
                    .encode(signing.verifying_key().to_bytes()),
            )
            .unwrap();
        trusted
            .save(&directory.path().join("trusted_publishers.toml"))
            .unwrap();
        manifest.security.signature = base64::engine::general_purpose::STANDARD
            .encode(signing.sign(&signing_digest(&manifest)).to_bytes());
        let store = Arc::new(
            RemoteUiPluginStore::open(directory.path(), directory.path(), runtime, vec![23; 32])
                .unwrap(),
        );
        let granted = CapabilitySet::from_spec(&manifest.capabilities);
        let granted_ui = manifest
            .ui
            .as_ref()
            .unwrap()
            .requested_capabilities
            .iter()
            .copied()
            .collect();
        store
            .install_disabled(manifest, &artifact, false, granted, granted_ui)
            .unwrap();

        let broker = RemoteUiBroker::default();
        store
            .smoke_test(
                "lifecycle-ui",
                Arc::clone(&supervisor),
                broker.producer_offer(),
            )
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .enable("lifecycle-ui", "session", Some(session_id))
            .unwrap();

        // A candidate that handshakes but publishes an undeclared initial
        // contribution must never replace the currently enabled artifact.
        let invalid_source = String::from_utf8(source.clone())
            .unwrap()
            .replace("lifecycle-ui.panel", "lifecycle-ui.undeclared");
        let invalid_artifact = archive(&[
            ("worker.mjs", invalid_source.as_bytes()),
            ("bin/native", b"native fixture update" as &[u8]),
        ]);
        let candidate_text = manifest_text
            .replace("version = \"1.0.0\"", "version = \"1.0.1\"")
            .replace(&checksum_of(&artifact), &checksum_of(&invalid_artifact));
        let mut candidate = parse_manifest(&candidate_text).unwrap();
        candidate.security.signature = base64::engine::general_purpose::STANDARD
            .encode(signing.sign(&signing_digest(&candidate)).to_bytes());
        let safe_staged = store
            .update("lifecycle-ui", candidate, &invalid_artifact, false)
            .unwrap();
        assert_eq!(safe_staged.update_permission_diff.as_deref(), Some(""));
        let safe_receipt = safe_staged.update_approval_receipt.unwrap();
        assert!(store
            .preflight_pending_update(
                "lifecycle-ui",
                &safe_receipt,
                Arc::clone(&supervisor),
                broker.producer_offer(),
            )
            .await
            .is_err());
        store.reject_update("lifecycle-ui", &safe_receipt).unwrap();
        let unchanged = store.status("lifecycle-ui").unwrap();
        assert_eq!(unchanged.version, "1.0.0");
        assert_eq!(unchanged.state, LifecycleState::Enabled);

        // Even a legacy/staged expanding candidate is preflighted again before
        // its one-shot approval receipt can be consumed. Failure leaves the
        // current enabled launch available and the receipt rejectable.
        let expanded_text = candidate_text
            .replace("version = \"1.0.1\"", "version = \"1.0.2\"")
            .replace(
                "\"workflow-read\", \"command-invoke\"",
                "\"workflow-read\", \"run-read\", \"command-invoke\"",
            );
        let mut expanded = parse_manifest(&expanded_text).unwrap();
        expanded.security.signature = base64::engine::general_purpose::STANDARD
            .encode(signing.sign(&signing_digest(&expanded)).to_bytes());
        let pending = store
            .update("lifecycle-ui", expanded, &invalid_artifact, false)
            .unwrap();
        assert_eq!(pending.state, LifecycleState::UpdateBlocked);
        let receipt = pending.update_approval_receipt.unwrap();
        assert!(store
            .preflight_pending_update(
                "lifecycle-ui",
                &receipt,
                Arc::clone(&supervisor),
                broker.producer_offer(),
            )
            .await
            .is_err());
        assert_eq!(store.launches_for(session_id).len(), 1);
        store.reject_update("lifecycle-ui", &receipt).unwrap();

        let client_id = ClientId::new();
        let mut renderer = broker.subscribe_renderer(session_id, client_id).unwrap();
        let mut capabilities = broker.producer_offer();
        capabilities.client = UiClientKind::from("terminal");
        let capabilities_message: UiWireMessage = serde_json::from_value(serde_json::json!({
            "type": "capabilities",
            "messageId": "renderer-capabilities",
            "capabilities": capabilities,
        }))
        .unwrap();
        broker
            .handle_renderer(
                session_id,
                client_id,
                ClientRole::Controller,
                capabilities_message,
            )
            .unwrap();

        let service = RemoteUiWorkerService::new(supervisor, store.clone());
        let (request_tx, mut request_rx) = tokio::sync::mpsc::channel(16);
        assert_eq!(
            service.ensure_session_target(
                session_id,
                UiTarget::Terminal,
                broker.clone(),
                request_tx,
            ),
            1
        );

        let subscriptions = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            let mut subscriptions = std::collections::BTreeMap::new();
            while subscriptions.len() < 4 {
                if let Some(UiWorkerRequest::Subscription { subscription, .. }) =
                    request_rx.recv().await
                {
                    subscriptions.insert(subscription.request.kind.clone(), subscription);
                }
            }
            subscriptions
        })
        .await
        .expect("real worker requested every governed projection");
        let command = &subscriptions["command"];
        assert_eq!(command.request.resource_id.as_deref(), Some("core.refresh"));
        assert!(subscriptions["context"].request.resource_id.is_none());
        assert_eq!(
            subscriptions["workflow"].request.resource_id.as_deref(),
            Some("lifecycle-workflow")
        );
        let artifact_request = &subscriptions["artifact"].request;
        assert_eq!(
            artifact_request.resource_id.as_deref(),
            Some("lifecycle-artifact")
        );
        assert_eq!(
            artifact_request.parameters.get("page"),
            Some(&serde_json::json!(1))
        );
        assert_eq!(
            artifact_request.parameters.get("pageSize"),
            Some(&serde_json::json!(16))
        );
        for (kind, subscription) in &subscriptions {
            let value = match kind.as_str() {
                "command" => serde_json::json!({
                    "id": "core.refresh",
                    "title": "Refresh",
                    "enabled": true,
                }),
                "context" => serde_json::json!({
                    "activeFile": "src/main.rs",
                    "openFiles": ["src/main.rs"],
                    "dirtyBuffers": [],
                    "diagnosticsRevision": 7,
                }),
                "workflow" => serde_json::json!({
                    "workflowRunId": "lifecycle-workflow",
                    "phase": "running",
                    "nodes": [],
                }),
                "artifact" => serde_json::json!({
                    "id": "lifecycle-artifact",
                    "mediaType": "text/plain",
                    "revision": 1,
                    "value": {
                        "contentBase64": "cGFnZQ==",
                        "range": { "offset": 16, "length": 4, "total": 20, "page": 1, "pageSize": 16 },
                    },
                }),
                _ => unreachable!(),
            };
            broker
                .deliver_projection(
                    session_id,
                    &subscription.producer,
                    UiProjectionUpdate {
                        subscription_id: subscription.request.subscription_id.clone(),
                        revision: Some(codypendent_protocol::UiRevision(1)),
                        removed: false,
                        value,
                    },
                )
                .unwrap();
        }

        let (document_id, saw_contribution) =
            tokio::time::timeout(std::time::Duration::from_secs(8), async {
                let mut document_id = None;
                let mut contribution = false;
                loop {
                    let frame = renderer.receiver.recv().await.unwrap();
                    let renderer_target = match &frame.target {
                        UiBrokerTarget::AllRenderers => true,
                        UiBrokerTarget::Renderer(id) => *id == client_id,
                        UiBrokerTarget::Producer(_) => false,
                    };
                    if !renderer_target {
                        continue;
                    }
                    if let Some(snapshot) = frame.message.snapshot {
                        document_id = Some(snapshot.document.document_id);
                    }
                    if !frame.message.contributions.is_empty() {
                        contribution = true;
                    }
                    if contribution {
                        if let Some(document_id) = document_id.take() {
                            break (document_id, true);
                        }
                    }
                }
            })
            .await
            .expect("renderer received worker snapshot and contribution");
        assert!(saw_contribution);

        let event: UiWireMessage = serde_json::from_value(serde_json::json!({
            "type": "event",
            "messageId": "renderer-action-event",
            "event": {
                "protocolVersion": { "major": 1, "minor": 0 },
                "eventId": "renderer-gesture",
                "documentId": document_id,
                "revision": 0,
                "targetId": "gesture",
                "type": "action",
                "payload": null,
            },
        }))
        .unwrap();
        broker
            .handle_renderer(session_id, client_id, ClientRole::Controller, event)
            .unwrap();

        let action = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                if let Some(UiWorkerRequest::Action { action, .. }) = request_rx.recv().await {
                    break action;
                }
            }
        })
        .await
        .expect("gesture-bound action reached daemon mediator");
        assert_eq!(action.invocation.action_id.as_str(), "core.refresh");
        broker
            .settle_action(
                session_id,
                &action.producer,
                UiActionResult {
                    invocation_id: UiEventId::from("lifecycle-invocation"),
                    status: "succeeded".to_owned(),
                    value: serde_json::json!({ "refreshed": true }),
                    error: None,
                },
            )
            .unwrap();

        let result_snapshot = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let frame = renderer.receiver.recv().await.unwrap();
                if let Some(snapshot) = frame.message.snapshot {
                    if snapshot.document.revision.0 == 1 {
                        break snapshot;
                    }
                }
            }
        })
        .await
        .expect("worker observed action result and published revision 1");
        assert_eq!(result_snapshot.document.revision.0, 1);

        let revoked = store.remove_trusted_publisher("test").unwrap();
        assert!(revoked.failures.is_empty());
        assert_eq!(revoked.plugins.len(), 1);
        assert_eq!(revoked.plugins[0].state, LifecycleState::Revoked);
        assert_eq!(service.stop_plugin("lifecycle-ui").len(), 1);
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                if matches!(
                    request_rx.recv().await,
                    Some(UiWorkerRequest::ProducerStopped { .. })
                ) {
                    break;
                }
            }
        })
        .await;
        assert!(stopped.is_ok(), "trust removal completes orderly teardown");
        assert_eq!(service.active_count(session_id), 0);
        assert_eq!(
            broker
                .disconnect_renderer(session_id, client_id)
                .remaining_total,
            0
        );

        // Explicit safe key rotation: trust removal revoked/stopped the old
        // record; after trusting a new key, only that revoked same-id record is
        // replaceable, and the authenticated old record is archived for audit.
        let rotated_signing = SigningKey::from_bytes(&[48_u8; 32]);
        let mut rotated_trust =
            TrustedPublishers::load(&directory.path().join("trusted_publishers.toml")).unwrap();
        rotated_trust
            .add(
                "test",
                &base64::engine::general_purpose::STANDARD
                    .encode(rotated_signing.verifying_key().to_bytes()),
            )
            .unwrap();
        rotated_trust
            .save(&directory.path().join("trusted_publishers.toml"))
            .unwrap();
        let rotated_artifact = archive(&[
            ("worker.mjs", source.as_slice()),
            ("bin/native", b"native fixture rotated" as &[u8]),
        ]);
        let rotated_text = manifest_text
            .replace("version = \"1.0.0\"", "version = \"2.0.0\"")
            .replace(&checksum_of(&artifact), &checksum_of(&rotated_artifact));
        let mut rotated_manifest = parse_manifest(&rotated_text).unwrap();
        rotated_manifest.security.signature = base64::engine::general_purpose::STANDARD.encode(
            rotated_signing
                .sign(&signing_digest(&rotated_manifest))
                .to_bytes(),
        );
        let rotated_granted = CapabilitySet::from_spec(&rotated_manifest.capabilities);
        let rotated_ui = rotated_manifest
            .ui
            .as_ref()
            .unwrap()
            .requested_capabilities
            .iter()
            .copied()
            .collect();
        let rotated = store
            .install_disabled(
                rotated_manifest,
                &rotated_artifact,
                false,
                rotated_granted,
                rotated_ui,
            )
            .unwrap();
        assert_eq!(rotated.version, "2.0.0");
        assert_eq!(rotated.state, LifecycleState::InstalledDisabled);
        assert_eq!(
            std::fs::read_dir(directory.path().join("plugins/remote-ui/records/archive"))
                .unwrap()
                .count(),
            1
        );
    }
}
