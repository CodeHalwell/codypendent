//! The plugin lifecycle (STEP 6.1).
//!
//! Chapter 05's lifecycle, enforced as a state machine rather than a convention:
//!
//! ```text
//! discover → inspect manifest → verify signature/checksum → evaluate permissions
//! → install disabled → sandbox smoke test → user enables at scope → monitor
//! → update with permission diff → revoke / remove
//! ```
//!
//! A plugin is **installed disabled** — it can do nothing until a human enables it
//! at a chosen scope. An **update** recomputes the permission diff against the
//! installed grant: an expansion is refused until re-approved (exit criterion 2).
//! Each installed plugin carries the trust record Chapter 05 requires (publisher,
//! content hash, signature status, requested capabilities, trust tier, installed
//! scope, revocation status), so retrieval and audit read trust *facts*, never the
//! plugin's self-description.

use crate::manifest::PluginManifest;
use crate::permission::{CapabilitySet, PermissionDiff};
use crate::verify::{verify_artifact, UnsignedPolicy, Verified, VerifyError};

/// The trust tier a plugin's items enter the registry at. Semantic relevance
/// never lifts this tier (Chapter 05 / the Phase 2 hard filter): an untrusted
/// plugin's tools are retrievable only where policy admits untrusted content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustTier {
    /// Signed by a trusted publisher.
    Trusted,
    /// Checksum-verified but unsigned (allowed only by explicit policy).
    Unsigned,
}

/// Where in its lifecycle a plugin sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// Verified + permission-evaluated, installed but inert. Nothing runs.
    InstalledDisabled,
    /// The sandbox smoke test passed (start, handshake, list tools, stop).
    SmokeTested,
    /// Enabled by a human at a chosen scope; the plugin's tools are live.
    Enabled,
    /// Update blocked: the new manifest expands permissions, awaiting re-approval.
    UpdateBlocked,
    /// Revoked / removed; the plugin is inert and its items deregistered.
    Revoked,
}

/// A lifecycle transition failure.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum LifecycleError {
    #[error("verification failed: {0}")]
    Verify(#[from] VerifyError),
    #[error("cannot {action} a plugin in state {state:?}")]
    IllegalTransition {
        action: &'static str,
        state: LifecycleState,
    },
    #[error("update blocked: it expands permissions and must be re-approved:\n{diff}")]
    UpdateExpandsPermissions { diff: String },
    #[error("plugin id/kind changed on update (was {old}, now {new}); reinstall required")]
    IdentityChanged { old: String, new: String },
    /// A granted capability is not one the manifest requested. A grant may only
    /// *narrow* the manifest's request, never widen it — otherwise a caller could
    /// smuggle an undeclared capability past the manifest into the sandbox profile
    /// (exit criterion 1).
    #[error("granted capability not requested by the manifest: {capability}")]
    GrantExceedsManifest { capability: String },
}

/// An installed plugin and its trust record. The `state` is the lifecycle
/// position; `granted` is the capability set the user approved (the profile is
/// derived from this, not from the manifest's request).
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub state: LifecycleState,
    pub trust: TrustTier,
    /// The checksum the artifact verified against (content hash, for the record).
    pub content_hash: String,
    /// Whether a real publisher signature verified.
    pub signed: bool,
    /// The capabilities the user granted (may narrow the manifest's request).
    pub granted: CapabilitySet,
    /// The scope the user enabled the plugin at (`None` until enabled).
    pub enabled_scope: Option<String>,
}

impl InstalledPlugin {
    /// **Install (disabled).** Verify the artifact, evaluate permissions, and
    /// record trust — but do not run anything. The plugin is inert until enabled.
    ///
    /// `granted` is the capability set the user approves at install; passing the
    /// manifest's full set grants everything requested, a subset withholds
    /// capabilities. Verification (checksum, signature/unsigned policy) happens
    /// here, before the plugin exists on disk in an installed state.
    pub fn install_disabled(
        manifest: PluginManifest,
        artifact: &[u8],
        publisher_key: Option<&[u8]>,
        unsigned: UnsignedPolicy,
        granted: CapabilitySet,
    ) -> Result<Self, LifecycleError> {
        let Verified { signed } = verify_artifact(&manifest, artifact, publisher_key, unsigned)?;
        // A grant may only narrow the manifest — reject any granted capability the
        // manifest did not request, so an undeclared capability can never reach the
        // sandbox profile through an over-broad grant.
        let requested = CapabilitySet::from_spec(&manifest.capabilities);
        for cap in granted.iter() {
            if !requested.grants(cap) {
                return Err(LifecycleError::GrantExceedsManifest {
                    capability: cap.to_string(),
                });
            }
        }
        let trust = if signed {
            TrustTier::Trusted
        } else {
            TrustTier::Unsigned
        };
        Ok(Self {
            content_hash: manifest.security.checksum.trim().to_string(),
            manifest,
            state: LifecycleState::InstalledDisabled,
            trust,
            signed,
            granted,
            enabled_scope: None,
        })
    }

    /// **Sandbox smoke test.** In production this starts the plugin inside the
    /// sandbox, handshakes, lists its tools, and stops it. Here it records the
    /// transition — the executor supplies the real round-trip result. Only an
    /// installed-disabled plugin can be smoke-tested.
    pub fn mark_smoke_tested(&mut self) -> Result<(), LifecycleError> {
        match self.state {
            LifecycleState::InstalledDisabled => {
                self.state = LifecycleState::SmokeTested;
                Ok(())
            }
            state => Err(LifecycleError::IllegalTransition {
                action: "smoke-test",
                state,
            }),
        }
    }

    /// **Enable at a scope.** A human turns the plugin on; its tools go live at
    /// the chosen scope. Requires a passed smoke test.
    pub fn enable(&mut self, scope: impl Into<String>) -> Result<(), LifecycleError> {
        match self.state {
            LifecycleState::SmokeTested | LifecycleState::UpdateBlocked => {
                self.state = LifecycleState::Enabled;
                self.enabled_scope = Some(scope.into());
                Ok(())
            }
            state => Err(LifecycleError::IllegalTransition {
                action: "enable",
                state,
            }),
        }
    }

    /// **Revoke / remove.** The plugin becomes inert and its items are
    /// deregistered. Legal from any state.
    pub fn revoke(&mut self) {
        self.state = LifecycleState::Revoked;
        self.enabled_scope = None;
    }

    /// Compute the permission diff a candidate update would introduce, without
    /// applying it. The TUI renders this to the user at the decision point.
    #[must_use]
    pub fn diff_update(&self, next: &PluginManifest) -> PermissionDiff {
        let next_set = CapabilitySet::from_spec(&next.capabilities);
        self.granted.diff_to(&next_set)
    }

    /// **Update.** Verify the new artifact and apply the update **only if it does
    /// not expand permissions** (exit criterion 2). An expansion returns
    /// [`LifecycleError::UpdateExpandsPermissions`] carrying the rendered diff and
    /// leaves the plugin `UpdateBlocked` — the operator must call
    /// [`Self::approve_update`] to accept the expanded grant.
    ///
    /// A permission-identical update (or one that only *narrows*) applies
    /// automatically. The plugin id and kind must not change — that is a new
    /// plugin, not an update.
    pub fn update(
        &mut self,
        next: PluginManifest,
        artifact: &[u8],
        publisher_key: Option<&[u8]>,
        unsigned: UnsignedPolicy,
    ) -> Result<PermissionDiff, LifecycleError> {
        if next.id != self.manifest.id {
            return Err(LifecycleError::IdentityChanged {
                old: self.manifest.id.clone(),
                new: next.id,
            });
        }
        if next.kind != self.manifest.kind {
            return Err(LifecycleError::IdentityChanged {
                old: self.manifest.kind.as_str().to_string(),
                new: next.kind.as_str().to_string(),
            });
        }
        let Verified { signed } = verify_artifact(&next, artifact, publisher_key, unsigned)?;
        let diff = self.diff_update(&next);
        if diff.expands_permissions() {
            // Refuse: record the pending manifest as blocked. The grant is
            // unchanged (the plugin keeps running under its old, narrower grant if
            // it was enabled) until a human approves.
            self.state = LifecycleState::UpdateBlocked;
            return Err(LifecycleError::UpdateExpandsPermissions {
                diff: diff.render(),
            });
        }
        // Identical or narrowing: apply. The granted set follows the new manifest
        // intersected down to what it declares (a narrowing update drops grants).
        let next_set = CapabilitySet::from_spec(&next.capabilities);
        self.apply_manifest(next, next_set, signed);
        Ok(diff)
    }

    /// **Approve a blocked update.** The human accepts the expanded permissions
    /// from a prior [`Self::update`] that returned
    /// [`LifecycleError::UpdateExpandsPermissions`]. Re-verifies and applies the
    /// new manifest with the (now approved) expanded grant.
    pub fn approve_update(
        &mut self,
        next: PluginManifest,
        artifact: &[u8],
        publisher_key: Option<&[u8]>,
        unsigned: UnsignedPolicy,
    ) -> Result<(), LifecycleError> {
        if next.id != self.manifest.id || next.kind != self.manifest.kind {
            return Err(LifecycleError::IdentityChanged {
                old: self.manifest.id.clone(),
                new: next.id,
            });
        }
        let Verified { signed } = verify_artifact(&next, artifact, publisher_key, unsigned)?;
        let next_set = CapabilitySet::from_spec(&next.capabilities);
        self.apply_manifest(next, next_set, signed);
        Ok(())
    }

    fn apply_manifest(&mut self, next: PluginManifest, granted: CapabilitySet, signed: bool) {
        self.content_hash = next.security.checksum.trim().to_string();
        self.manifest = next;
        self.granted = granted;
        self.signed = signed;
        self.trust = if signed {
            TrustTier::Trusted
        } else {
            TrustTier::Unsigned
        };
        // An update leaves the plugin enabled if it was enabled (a permission-safe
        // update is transparent); otherwise it returns to installed-disabled.
        if self.state != LifecycleState::Enabled {
            self.state = LifecycleState::InstalledDisabled;
        }
    }

    /// Whether the plugin's tools are currently live (enabled and not revoked).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state == LifecycleState::Enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;
    use crate::verify::checksum_of;

    fn manifest(version: &str, network: &[&str]) -> (PluginManifest, Vec<u8>) {
        let artifact = format!("plugin bytes {version}").into_bytes();
        let net = network
            .iter()
            .map(|h| format!("\"{h}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let toml = format!(
            r#"
schema_version = 1
id = "github"
name = "GitHub"
version = "{version}"
kind = "native-process"
publisher = "codypendent-project"
[runtime]
command = "codypendent-plugin-github"
[capabilities]
network = [{net}]
[security]
checksum = "{}"
signature = "set-during-packaging"
"#,
            checksum_of(&artifact)
        );
        (parse_manifest(&toml).unwrap(), artifact)
    }

    fn install(version: &str, network: &[&str]) -> InstalledPlugin {
        let (m, artifact) = manifest(version, network);
        let granted = CapabilitySet::from_spec(&m.capabilities);
        InstalledPlugin::install_disabled(m, &artifact, None, UnsignedPolicy::Allow, granted)
            .expect("installs")
    }

    #[test]
    fn installs_disabled_and_inert() {
        let p = install("0.1.0", &["api.github.com:443"]);
        assert_eq!(p.state, LifecycleState::InstalledDisabled);
        assert!(!p.is_active(), "a freshly installed plugin does nothing");
        assert_eq!(p.trust, TrustTier::Unsigned);
    }

    #[test]
    fn a_narrowing_grant_is_accepted() {
        // The manifest requests api.github.com; the user withholds it (grants
        // nothing). A narrowing grant is fine.
        let (m, artifact) = manifest("0.1.0", &["api.github.com:443"]);
        let narrowed = CapabilitySet::default();
        let p =
            InstalledPlugin::install_disabled(m, &artifact, None, UnsignedPolicy::Allow, narrowed)
                .expect("a narrowing grant installs");
        assert!(p.granted.is_empty());
    }

    #[test]
    fn a_grant_the_manifest_did_not_request_is_rejected() {
        // The manifest requests only api.github.com; a caller tries to grant a
        // filesystem read the manifest never declared. It must be refused so an
        // undeclared capability can't reach the sandbox profile (exit criterion 1).
        let (m, artifact) = manifest("0.1.0", &["api.github.com:443"]);
        let smuggled = CapabilitySet::from_spec(&crate::manifest::CapabilitiesSpec {
            filesystem_read: vec!["/home/user/.ssh".into()],
            network: vec!["api.github.com:443".into()],
            ..Default::default()
        });
        let err =
            InstalledPlugin::install_disabled(m, &artifact, None, UnsignedPolicy::Allow, smuggled)
                .unwrap_err();
        assert!(
            matches!(err, LifecycleError::GrantExceedsManifest { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn full_lifecycle_to_enabled() {
        let mut p = install("0.1.0", &["api.github.com:443"]);
        p.mark_smoke_tested().unwrap();
        assert_eq!(p.state, LifecycleState::SmokeTested);
        p.enable("repository").unwrap();
        assert!(p.is_active());
        assert_eq!(p.enabled_scope.as_deref(), Some("repository"));
    }

    #[test]
    fn cannot_enable_before_smoke_test() {
        let mut p = install("0.1.0", &["api.github.com:443"]);
        assert!(matches!(
            p.enable("repository"),
            Err(LifecycleError::IllegalTransition {
                action: "enable",
                ..
            })
        ));
    }

    #[test]
    fn permission_identical_update_auto_applies() {
        let mut p = install("0.1.0", &["api.github.com:443"]);
        p.mark_smoke_tested().unwrap();
        p.enable("repository").unwrap();
        let (next, artifact) = manifest("0.2.0", &["api.github.com:443"]);
        let diff = p
            .update(next, &artifact, None, UnsignedPolicy::Allow)
            .unwrap();
        assert!(diff.is_identical());
        assert_eq!(p.manifest.version, "0.2.0");
        assert!(p.is_active(), "a safe update stays enabled");
    }

    #[test]
    fn permission_expanding_update_is_blocked() {
        let mut p = install("0.1.0", &["api.github.com:443"]);
        p.mark_smoke_tested().unwrap();
        p.enable("repository").unwrap();
        let (next, artifact) = manifest("0.2.0", &["api.github.com:443", "uploads.github.com:443"]);
        let err = p
            .update(next, &artifact, None, UnsignedPolicy::Allow)
            .unwrap_err();
        match err {
            LifecycleError::UpdateExpandsPermissions { diff } => {
                assert_eq!(diff, "+ network: uploads.github.com:443");
            }
            other => panic!("expected an expansion block, got {other:?}"),
        }
        assert_eq!(p.state, LifecycleState::UpdateBlocked);
        // The grant did NOT change — still the old, narrower manifest.
        assert_eq!(p.manifest.version, "0.1.0");
        assert!(p.granted.grants(&crate::permission::Capability::Network(
            "api.github.com:443".into()
        )));
        assert!(!p.granted.grants(&crate::permission::Capability::Network(
            "uploads.github.com:443".into()
        )));
    }

    #[test]
    fn approving_a_blocked_update_applies_the_expanded_grant() {
        let mut p = install("0.1.0", &["api.github.com:443"]);
        p.mark_smoke_tested().unwrap();
        p.enable("repository").unwrap();
        let (next, artifact) = manifest("0.2.0", &["api.github.com:443", "uploads.github.com:443"]);
        // First the update is blocked...
        let _ = p
            .update(next.clone(), &artifact, None, UnsignedPolicy::Allow)
            .unwrap_err();
        assert_eq!(p.state, LifecycleState::UpdateBlocked);
        // ...then the human approves it.
        p.approve_update(next, &artifact, None, UnsignedPolicy::Allow)
            .unwrap();
        assert_eq!(p.manifest.version, "0.2.0");
        assert!(p.granted.grants(&crate::permission::Capability::Network(
            "uploads.github.com:443".into()
        )));
    }

    #[test]
    fn narrowing_update_applies_without_approval() {
        let mut p = install("0.1.0", &["api.github.com:443", "uploads.github.com:443"]);
        let (next, artifact) = manifest("0.2.0", &["api.github.com:443"]);
        let diff = p
            .update(next, &artifact, None, UnsignedPolicy::Allow)
            .unwrap();
        assert!(!diff.expands_permissions());
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(p.manifest.version, "0.2.0");
    }

    #[test]
    fn update_with_a_bad_checksum_is_rejected_before_diffing() {
        let mut p = install("0.1.0", &["api.github.com:443"]);
        let (next, _artifact) = manifest("0.2.0", &["api.github.com:443"]);
        let err = p
            .update(next, b"tampered", None, UnsignedPolicy::Allow)
            .unwrap_err();
        assert!(matches!(
            err,
            LifecycleError::Verify(VerifyError::ChecksumMismatch { .. })
        ));
        assert_eq!(p.manifest.version, "0.1.0");
    }

    #[test]
    fn update_that_changes_identity_is_rejected() {
        let mut p = install("0.1.0", &["api.github.com:443"]);
        let (mut next, artifact) = manifest("0.2.0", &["api.github.com:443"]);
        next.id = "totally-different".into();
        assert!(matches!(
            p.update(next, &artifact, None, UnsignedPolicy::Allow),
            Err(LifecycleError::IdentityChanged { .. })
        ));
    }

    #[test]
    fn revoke_makes_the_plugin_inert() {
        let mut p = install("0.1.0", &["api.github.com:443"]);
        p.mark_smoke_tested().unwrap();
        p.enable("repository").unwrap();
        p.revoke();
        assert_eq!(p.state, LifecycleState::Revoked);
        assert!(!p.is_active());
        assert!(p.enabled_scope.is_none());
    }
}
