//! Runner agent daemon coordinating lease claim, execution, logs, artifacts, and attestations.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tracing::{error, info, warn};

use crate::attestation::sign_attestation;
use crate::backend::RunnerBackend;
use crate::client::{ControlPlaneClient, ObjectStoreClient};
use crate::identity::RunnerIdentity;
use crate::log_streamer::LogStreamer;
use crate::materialize::{MaterializeLimits, Materializer};
use crate::types::{
    AttestationOutput, AttestationStatement, AttestationVerifyResult, ClaimRequest, JobClaim,
    OutputDeclaration, OutputUpload, ReleaseRequest, RenewRequest, RunnerError,
};
use crate::workspace::{WorkspaceGuard, WorkspaceManager};

const MAX_SINGLE_OUTPUT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_UPLOAD_JOURNAL_BYTES: u64 = 1024 * 1024;
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(10);
const LEASE_RENEW_TIMEOUT: Duration = Duration::from_secs(5);
const UPLOAD_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(1),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadJournal {
    version: u32,
    job_id: uuid::Uuid,
    attempt_id: uuid::Uuid,
    job_spec_hash: String,
    started_at: String,
    exit_code: i32,
    outputs: Vec<JournalOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalOutput {
    name: String,
    content_hash: String,
    byte_length: u64,
    media_type: String,
    object_key: String,
    classification: String,
    object_stored: bool,
    registered: bool,
}

/// The runner agent daemon.
pub struct RunnerAgent {
    pub identity: RunnerIdentity,
    pub control_plane: Arc<dyn ControlPlaneClient>,
    pub object_store: Arc<dyn ObjectStoreClient>,
    pub workspace_manager: WorkspaceManager,
    pub backend: Arc<dyn RunnerBackend>,
    pub materialize_limits: MaterializeLimits,
}

impl RunnerAgent {
    /// Create a new runner agent daemon.
    #[must_use]
    pub fn new(
        identity: RunnerIdentity,
        control_plane: Arc<dyn ControlPlaneClient>,
        object_store: Arc<dyn ObjectStoreClient>,
        workspace_manager: WorkspaceManager,
        backend: Arc<dyn RunnerBackend>,
    ) -> Self {
        Self {
            identity,
            control_plane,
            object_store,
            workspace_manager,
            backend,
            materialize_limits: MaterializeLimits::default(),
        }
    }

    /// Register with control plane and run the claim loop until shutdown is signaled.
    pub async fn run(&self, mut shutdown_rx: watch::Receiver<bool>) -> Result<(), RunnerError> {
        info!(
            runner_id = %self.identity.id,
            name = %self.identity.name,
            backend = %self.backend.name(),
            "Registering runner agent with control plane"
        );

        self.control_plane.register(&self.identity).await?;

        let claim_req = ClaimRequest {
            runner_id: self.identity.id,
            organization_id: self.identity.organization_id,
            capabilities: self.identity.capabilities.clone(),
            max_concurrency: 1,
        };

        while !*shutdown_rx.borrow() {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    info!("Shutdown signaled; stopping runner agent claim loop");
                    break;
                }
                claim_res = self.control_plane.claim(&claim_req) => {
                    match claim_res {
                        Ok(Some(claim)) => {
                            info!(job_id = %claim.job_id, attempt_id = %claim.attempt_id, "Claimed job lease");
                            if let Err(e) = self.execute_claim(claim).await {
                                error!(error = %e, "Failed to execute claimed job");
                            }
                        }
                        Ok(None) => {
                            // Queue empty; sleep briefly before polling again
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "Error claiming job; retrying after backoff");
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Execute a single claimed job lease end-to-end.
    pub async fn execute_claim(
        &self,
        claim: JobClaim,
    ) -> Result<AttestationVerifyResult, RunnerError> {
        validate_claim_integrity(&claim)?;
        let journal_path = self
            .workspace_manager
            .upload_journal_path(claim.job_id, claim.attempt_id);
        let mut upload_journal = load_upload_journal(&journal_path, &claim)?;
        let started_at = upload_journal.as_ref().map_or_else(
            || Utc::now().to_rfc3339(),
            |journal| journal.started_at.clone(),
        );

        // 1. Prepare isolated workspace
        let mut workspace = if upload_journal.is_some() {
            self.workspace_manager
                .resume_workspace(claim.job_id, claim.attempt_id)?
        } else {
            self.workspace_manager
                .create_workspace(claim.job_id, claim.attempt_id)?
        };

        // 2. Setup cancellation watch and background lease renewal
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let lease_id = claim.lease_id;
        let lease_token = claim.lease_token.clone();
        let control_plane = self.control_plane.clone();
        let lease_generation = Arc::new(Mutex::new(claim.lease_generation));
        let heartbeat_generation = lease_generation.clone();
        let initial_expiry = claim.expires_at;

        let heartbeat_handle = tokio::spawn(async move {
            let mut expires_at = initial_expiry;

            loop {
                let until_expiry = duration_until(expires_at);
                if until_expiry.is_zero() {
                    let _ = cancel_tx.send(true);
                    break;
                }
                tokio::time::sleep(LEASE_RENEW_INTERVAL.min(until_expiry)).await;
                if Utc::now() >= expires_at {
                    let _ = cancel_tx.send(true);
                    break;
                }

                let mut generation = heartbeat_generation.lock().await;
                let req = RenewRequest {
                    lease_id,
                    generation: *generation,
                    lease_token: lease_token.clone(),
                };

                let renewal_budget = LEASE_RENEW_TIMEOUT.min(duration_until(expires_at));
                match tokio::time::timeout(renewal_budget, control_plane.renew_lease(&req)).await {
                    Ok(Ok(resp))
                        if resp.renewed
                            && resp.new_generation > *generation
                            && resp.new_expires_at > Utc::now() =>
                    {
                        *generation = resp.new_generation;
                        expires_at = resp.new_expires_at;
                        if resp.cancel_requested {
                            let _ = cancel_tx.send(true);
                            break;
                        }
                    }
                    Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
                        // A syntactically successful response that did not renew
                        // the lease, a transport failure, and a hung renewal are
                        // all lost-lease conditions. Never execute beyond one.
                        let _ = cancel_tx.send(true);
                        break;
                    }
                }
            }
        });

        // 3. Reject untrusted output metadata before executing the job.
        let preparation_res = validate_output_names(&claim.job_spec.outputs)
            .and_then(|_| validate_data_classification(&claim.data_classification).map(|_| ()));
        let materialize_res = match (upload_journal.is_some(), preparation_res) {
            (_, Err(error)) => Err(error),
            (true, Ok(())) => Ok(()),
            (false, Ok(())) => self.materialize_inputs(&claim, &workspace).await,
        };

        let mut log_streamer =
            LogStreamer::new(claim.attempt_id, claim.job_spec.resources.maximum_output_mb);

        let (exit_code, mut result_state) = if let Some(journal) = &upload_journal {
            (journal.exit_code, "succeeded")
        } else {
            match materialize_res {
                Ok(_) => {
                    // 4. Execute command through backend
                    match self
                        .backend
                        .execute(&claim.job_spec, &workspace, cancel_rx)
                        .await
                    {
                        Ok(outcome) => {
                            for chunk in log_streamer.ingest_stdout(&outcome.stdout) {
                                let _ = self.control_plane.stream_logs(chunk).await;
                            }
                            for chunk in log_streamer.ingest_stderr(&outcome.stderr) {
                                let _ = self.control_plane.stream_logs(chunk).await;
                            }
                            for chunk in log_streamer.flush() {
                                let _ = self.control_plane.stream_logs(chunk).await;
                            }

                            let res = if outcome.exit_code == 0 {
                                "succeeded"
                            } else {
                                "failed"
                            };
                            (outcome.exit_code, res)
                        }
                        Err(RunnerError::Cancelled(_)) => {
                            let _ = log_streamer.ingest_stderr(b"Job cancelled mid-execution");
                            for chunk in log_streamer.flush() {
                                let _ = self.control_plane.stream_logs(chunk).await;
                            }
                            (-1, "cancelled")
                        }
                        Err(e) => {
                            let err_msg = format!("Backend execution error: {e}");
                            let _ = log_streamer.ingest_stderr(err_msg.as_bytes());
                            for chunk in log_streamer.flush() {
                                let _ = self.control_plane.stream_logs(chunk).await;
                            }
                            (-1, "failed")
                        }
                    }
                }
                Err(e) => {
                    let err_msg = format!("Job preparation error: {e}");
                    let _ = log_streamer.ingest_stderr(err_msg.as_bytes());
                    for chunk in log_streamer.flush() {
                        let _ = self.control_plane.stream_logs(chunk).await;
                    }
                    (-1, "failed")
                }
            }
        };

        if result_state == "succeeded" && upload_journal.is_none() {
            let journal = UploadJournal {
                version: 1,
                job_id: claim.job_id,
                attempt_id: claim.attempt_id,
                job_spec_hash: claim.job_spec_hash.clone(),
                started_at: started_at.clone(),
                exit_code,
                outputs: Vec::new(),
            };
            if let Err(error) = persist_upload_journal(&journal_path, &journal) {
                workspace.preserve_for_resume();
                heartbeat_handle.abort();
                return Err(error);
            }
            upload_journal = Some(journal);
        }

        // 5. Gather and upload declared outputs
        let mut attestation_outputs = Vec::new();
        if result_state == "succeeded" {
            match self
                .harvest_outputs(
                    &claim,
                    &workspace,
                    claim.job_spec.resources.maximum_output_mb,
                    upload_journal
                        .as_mut()
                        .map(|journal| (journal, journal_path.as_path())),
                )
                .await
            {
                Ok(outputs) => attestation_outputs = outputs,
                Err(error @ (RunnerError::TransientOutput(_) | RunnerError::ResumableState(_))) => {
                    workspace.preserve_for_resume();
                    heartbeat_handle.abort();
                    return Err(error);
                }
                Err(error) => {
                    warn!(
                        job_id = %claim.job_id,
                        attempt_id = %claim.attempt_id,
                        error = %error,
                        "Output harvesting failed"
                    );
                    let message = format!("Output harvesting error: {error}");
                    for chunk in log_streamer.ingest_stderr(message.as_bytes()) {
                        let _ = self.control_plane.stream_logs(chunk).await;
                    }
                    for chunk in log_streamer.flush() {
                        let _ = self.control_plane.stream_logs(chunk).await;
                    }
                    result_state = "failed";
                }
            }
        }

        // 6. Sign and submit attestation
        let ended_at = Utc::now().to_rfc3339();
        // Hold the generation lock through submission so a renewal cannot make
        // the signed lease generation stale while the attestation is in flight.
        // Renew once immediately before signing, then bound submission well
        // below the renewed lease duration.
        let mut generation = lease_generation.lock().await;
        let final_renewal = tokio::time::timeout(
            LEASE_RENEW_TIMEOUT,
            self.control_plane.renew_lease(&RenewRequest {
                lease_id: claim.lease_id,
                generation: *generation,
                lease_token: claim.lease_token.clone(),
            }),
        )
        .await;
        let final_renewal = match final_renewal {
            Ok(Ok(response))
                if response.renewed
                    && response.new_generation > *generation
                    && response.new_expires_at > Utc::now() =>
            {
                response
            }
            Ok(Ok(_)) => {
                drop(generation);
                heartbeat_handle.abort();
                if upload_journal.is_some() {
                    workspace.preserve_for_resume();
                }
                return Err(RunnerError::ControlPlane(
                    "control plane did not grant a fresh lease generation for attestation"
                        .to_string(),
                ));
            }
            Ok(Err(error)) => {
                drop(generation);
                heartbeat_handle.abort();
                if upload_journal.is_some() {
                    workspace.preserve_for_resume();
                }
                return Err(error);
            }
            Err(_) => {
                drop(generation);
                heartbeat_handle.abort();
                if upload_journal.is_some() {
                    workspace.preserve_for_resume();
                }
                return Err(RunnerError::ControlPlane(
                    "final lease renewal exceeded its timeout".to_string(),
                ));
            }
        };
        *generation = final_renewal.new_generation;
        if final_renewal.cancel_requested {
            result_state = "cancelled";
        }
        let statement = AttestationStatement {
            job_id: claim.job_id,
            job_spec_hash: claim.job_spec_hash.clone(),
            attempt_id: claim.attempt_id,
            attempt_number: claim.attempt_number,
            lease_id: claim.lease_id,
            lease_generation: *generation,
            runner_id: self.identity.id,
            image_digest: self
                .identity
                .capabilities
                .image_digest
                .clone()
                .unwrap_or_else(|| "none".to_string()),
            input_manifest_hash: claim.input_manifest_hash.clone(),
            outputs: attestation_outputs,
            started_at,
            ended_at,
            exit_code,
            result: result_state.to_string(),
        };

        let attestation = sign_attestation(statement, &self.identity);
        let verify_result = tokio::time::timeout(
            Duration::from_secs(30),
            self.control_plane.submit_attestation(&attestation),
        )
        .await
        .map_err(|_| {
            RunnerError::ControlPlane("attestation submission exceeded 30 seconds".to_string())
        })
        .and_then(|result| result);
        drop(generation);
        heartbeat_handle.abort();
        let verify_result = match verify_result {
            Ok(result) => result,
            Err(error) => {
                if upload_journal.is_some() {
                    workspace.preserve_for_resume();
                }
                return Err(error);
            }
        };

        // 7. Teardown workspace and release lease
        workspace.teardown()?;
        remove_upload_journal(&journal_path)?;

        let release_req = ReleaseRequest {
            lease_id: claim.lease_id,
            attempt_id: claim.attempt_id,
            reason: result_state.to_string(),
            lease_token: claim.lease_token,
        };
        let _ = self.control_plane.release_lease(&release_req).await;

        Ok(verify_result)
    }

    async fn harvest_outputs(
        &self,
        claim: &JobClaim,
        workspace: &WorkspaceGuard,
        requested_limit_mb: u64,
        mut journal: Option<(&mut UploadJournal, &Path)>,
    ) -> Result<Vec<AttestationOutput>, RunnerError> {
        validate_output_names(&claim.job_spec.outputs)?;
        let classification = validate_data_classification(&claim.data_classification)?.to_string();
        let relative_paths = claim
            .job_spec
            .outputs
            .iter()
            .map(|declaration| validate_output_path(&declaration.path))
            .collect::<Result<Vec<_>, _>>()?;
        let requested_limit = requested_limit_mb.checked_mul(1024 * 1024).ok_or_else(|| {
            RunnerError::Output("maximum output size overflows byte count".to_string())
        })?;
        let read_limit = requested_limit.min(MAX_SINGLE_OUTPUT_BYTES);
        let mut remaining_bytes = read_limit;
        let mut outputs = Vec::new();

        for (declaration, relative) in claim.job_spec.outputs.iter().zip(relative_paths) {
            let existing = journal.as_ref().and_then(|(journal, _)| {
                journal
                    .outputs
                    .iter()
                    .find(|output| output.name == declaration.name)
                    .cloned()
            });
            if let Some(existing) = &existing {
                remaining_bytes = remaining_bytes
                    .checked_sub(existing.byte_length)
                    .ok_or_else(|| {
                        RunnerError::Output(
                            "resumed outputs exceed the aggregate output limit".to_string(),
                        )
                    })?;
                if existing.registered {
                    outputs.push(AttestationOutput {
                        name: existing.name.clone(),
                        content_hash: existing.content_hash.clone(),
                        byte_length: existing.byte_length,
                    });
                    continue;
                }
            }

            let (bytes, mut journal_output) = if let Some(existing) = existing {
                let bytes = if existing.object_stored {
                    None
                } else {
                    let bytes = read_required_or_optional_output(
                        workspace,
                        &relative,
                        remaining_bytes.saturating_add(existing.byte_length),
                        declaration,
                    )?;
                    let bytes = bytes.ok_or_else(|| {
                        RunnerError::Output(format!(
                            "resumable output {:?} disappeared after execution",
                            declaration.name
                        ))
                    })?;
                    let actual_hash = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
                    if actual_hash != existing.content_hash
                        || bytes.len() as u64 != existing.byte_length
                    {
                        return Err(RunnerError::Output(format!(
                            "resumable output {:?} changed after execution",
                            declaration.name
                        )));
                    }
                    Some(bytes)
                };
                (bytes, existing)
            } else {
                let bytes = read_required_or_optional_output(
                    workspace,
                    &relative,
                    remaining_bytes,
                    declaration,
                )?;
                let Some(bytes) = bytes else { continue };
                remaining_bytes -= bytes.len() as u64;
                let output = JournalOutput {
                    name: declaration.name.clone(),
                    content_hash: format!("sha256:{}", hex::encode(Sha256::digest(&bytes))),
                    byte_length: bytes.len() as u64,
                    media_type: declaration.media_type.clone(),
                    object_key: format!(
                        "artifacts/{}/{}/{}",
                        claim.job_id, claim.attempt_id, declaration.name
                    ),
                    classification: classification.clone(),
                    object_stored: false,
                    registered: false,
                };
                if let Some((journal, path)) = journal.as_mut() {
                    journal.outputs.push(output.clone());
                    persist_upload_journal(path, journal)?;
                }
                (Some(bytes), output)
            };

            if !journal_output.object_stored {
                let bytes = bytes.as_deref().ok_or_else(|| {
                    RunnerError::Output(format!(
                        "resumable output {:?} has no stored object or local bytes",
                        declaration.name
                    ))
                })?;
                self.upload_artifact_with_retry(
                    &journal_output.object_key,
                    bytes,
                    &declaration.name,
                )
                .await?;
                journal_output.object_stored = true;
                update_journal_output(&mut journal, &journal_output)?;
            }

            let upload_req = OutputUpload {
                attempt_id: claim.attempt_id,
                name: journal_output.name.clone(),
                content_hash: journal_output.content_hash.clone(),
                byte_length: journal_output.byte_length,
                media_type: journal_output.media_type.clone(),
                object_key: journal_output.object_key.clone(),
                classification: journal_output.classification.clone(),
            };
            self.register_output_with_retry(&upload_req).await?;
            journal_output.registered = true;
            update_journal_output(&mut journal, &journal_output)?;

            outputs.push(AttestationOutput {
                name: journal_output.name,
                content_hash: journal_output.content_hash,
                byte_length: journal_output.byte_length,
            });
        }

        Ok(outputs)
    }

    async fn upload_artifact_with_retry(
        &self,
        object_key: &str,
        bytes: &[u8],
        output_name: &str,
    ) -> Result<(), RunnerError> {
        let mut last_error = None;
        for (attempt, delay) in UPLOAD_RETRY_DELAYS.iter().enumerate() {
            match self.object_store.upload_artifact(object_key, bytes).await {
                Ok(()) => return Ok(()),
                Err(error) if transient_transport_error(&error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
            if attempt + 1 < UPLOAD_RETRY_DELAYS.len() {
                tokio::time::sleep(*delay).await;
            }
        }
        Err(RunnerError::TransientOutput(format!(
            "failed to store output {output_name:?} after retries: {}",
            last_error.expect("retry loop always records an error")
        )))
    }

    async fn register_output_with_retry(&self, upload: &OutputUpload) -> Result<(), RunnerError> {
        let mut last_error = None;
        for (attempt, delay) in UPLOAD_RETRY_DELAYS.iter().enumerate() {
            match self.control_plane.upload_output(upload).await {
                Ok(()) => return Ok(()),
                Err(error) if transient_transport_error(&error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
            if attempt + 1 < UPLOAD_RETRY_DELAYS.len() {
                tokio::time::sleep(*delay).await;
            }
        }
        Err(RunnerError::TransientOutput(format!(
            "failed to register output {:?} after retries: {}",
            upload.name,
            last_error.expect("retry loop always records an error")
        )))
    }

    async fn materialize_inputs(
        &self,
        claim: &JobClaim,
        workspace: &WorkspaceGuard,
    ) -> Result<(), RunnerError> {
        if claim.input_manifest_hash.is_empty() || claim.input_manifest_hash == "none" {
            return Ok(());
        }

        let bundle_bytes = self
            .object_store
            .fetch_input_bundle(&claim.input_manifest_hash)
            .await?;

        let materializer = Materializer::new(self.materialize_limits.clone());
        materializer.materialize_bytes(&bundle_bytes, &workspace.source_dir, None)?;

        Ok(())
    }
}

fn validate_claim_integrity(claim: &JobClaim) -> Result<(), RunnerError> {
    if claim.expires_at <= Utc::now() {
        return Err(RunnerError::LeaseExpired(claim.expires_at));
    }
    let computed_job_spec_hash = claim.job_spec.compute_hash();
    if claim.job_spec_hash != computed_job_spec_hash {
        return Err(RunnerError::Attestation(format!(
            "claimed job_spec_hash does not bind the executable job spec: expected {computed_job_spec_hash}, got {}",
            claim.job_spec_hash
        )));
    }
    if claim.input_manifest_hash != claim.job_spec.input_manifest_hash {
        return Err(RunnerError::Attestation(
            "claim and job spec disagree on the input manifest hash".to_string(),
        ));
    }
    Ok(())
}

fn duration_until(deadline: chrono::DateTime<Utc>) -> Duration {
    deadline
        .signed_duration_since(Utc::now())
        .to_std()
        .unwrap_or(Duration::ZERO)
}

fn read_required_or_optional_output(
    workspace: &WorkspaceGuard,
    relative: &Path,
    limit: u64,
    declaration: &OutputDeclaration,
) -> Result<Option<Vec<u8>>, RunnerError> {
    match read_declared_output(workspace, relative, limit) {
        Ok(Some(bytes)) => Ok(Some(bytes)),
        Ok(None) if !declaration.required => Ok(None),
        Ok(None) => Err(RunnerError::Output(format!(
            "required output {:?} is missing at {:?}",
            declaration.name, declaration.path
        ))),
        Err(error) => Err(RunnerError::Output(format!(
            "failed to read output {:?} at {:?}: {error}",
            declaration.name, declaration.path
        ))),
    }
}

fn transient_transport_error(error: &RunnerError) -> bool {
    matches!(
        error,
        RunnerError::ObjectStore(_) | RunnerError::ControlPlane(_) | RunnerError::Io(_)
    )
}

fn load_upload_journal(
    path: &Path,
    claim: &JobClaim,
) -> Result<Option<UploadJournal>, RunnerError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RunnerError::Workspace(format!(
                "failed to inspect upload journal: {error}"
            )))
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_UPLOAD_JOURNAL_BYTES
    {
        return Err(RunnerError::Workspace(
            "upload journal is not a bounded regular file".to_string(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|file| {
            file.take(MAX_UPLOAD_JOURNAL_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| {
            RunnerError::Workspace(format!("failed to read upload journal: {error}"))
        })?;
    if bytes.len() as u64 > MAX_UPLOAD_JOURNAL_BYTES {
        return Err(RunnerError::Workspace(
            "upload journal exceeded its read cap".to_string(),
        ));
    }
    let journal: UploadJournal = serde_json::from_slice(&bytes)
        .map_err(|error| RunnerError::Workspace(format!("upload journal is malformed: {error}")))?;
    validate_upload_journal(&journal, claim)?;
    Ok(Some(journal))
}

fn validate_upload_journal(journal: &UploadJournal, claim: &JobClaim) -> Result<(), RunnerError> {
    if journal.version != 1
        || journal.job_id != claim.job_id
        || journal.attempt_id != claim.attempt_id
        || journal.job_spec_hash != claim.job_spec_hash
        || journal.exit_code != 0
        || chrono::DateTime::parse_from_rfc3339(&journal.started_at).is_err()
    {
        return Err(RunnerError::Workspace(
            "upload journal does not match the claimed successful attempt".to_string(),
        ));
    }
    validate_output_names(&claim.job_spec.outputs)?;
    let classification = validate_data_classification(&claim.data_classification)?;
    let mut names = HashSet::with_capacity(journal.outputs.len());
    for output in &journal.outputs {
        let declaration = claim
            .job_spec
            .outputs
            .iter()
            .find(|declaration| declaration.name == output.name)
            .ok_or_else(|| {
                RunnerError::Workspace(format!(
                    "upload journal contains undeclared output {:?}",
                    output.name
                ))
            })?;
        let expected_key = format!(
            "artifacts/{}/{}/{}",
            claim.job_id, claim.attempt_id, output.name
        );
        if !names.insert(output.name.as_str())
            || output.media_type != declaration.media_type
            || output.classification != classification
            || output.object_key != expected_key
            || output.registered && !output.object_stored
            || !valid_sha256_hash(&output.content_hash)
        {
            return Err(RunnerError::Workspace(format!(
                "upload journal metadata is invalid for output {:?}",
                output.name
            )));
        }
    }
    Ok(())
}

fn valid_sha256_hash(hash: &str) -> bool {
    hash.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn persist_upload_journal(path: &Path, journal: &UploadJournal) -> Result<(), RunnerError> {
    let parent = path.parent().ok_or_else(|| {
        RunnerError::ResumableState("upload journal path has no parent".to_string())
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        RunnerError::ResumableState(format!(
            "failed to create upload journal directory: {error}"
        ))
    })?;
    let bytes = serde_json::to_vec(journal).map_err(|error| {
        RunnerError::ResumableState(format!("failed to serialize upload journal: {error}"))
    })?;
    if bytes.len() as u64 > MAX_UPLOAD_JOURNAL_BYTES {
        return Err(RunnerError::ResumableState(
            "upload journal exceeds its size cap".to_string(),
        ));
    }
    let temporary = parent.join(format!(".journal-{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| -> Result<(), RunnerError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|error| {
            RunnerError::ResumableState(format!("failed to create upload journal: {error}"))
        })?;
        file.write_all(&bytes).map_err(|error| {
            RunnerError::ResumableState(format!("failed to write upload journal: {error}"))
        })?;
        file.sync_all().map_err(|error| {
            RunnerError::ResumableState(format!("failed to sync upload journal: {error}"))
        })?;
        fs::rename(&temporary, path).map_err(|error| {
            RunnerError::ResumableState(format!("failed to commit upload journal: {error}"))
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                RunnerError::ResumableState(format!(
                    "failed to sync upload journal directory: {error}"
                ))
            })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn update_journal_output(
    journal: &mut Option<(&mut UploadJournal, &Path)>,
    output: &JournalOutput,
) -> Result<(), RunnerError> {
    let Some((journal, path)) = journal.as_mut() else {
        return Ok(());
    };
    let stored = journal
        .outputs
        .iter_mut()
        .find(|stored| stored.name == output.name)
        .ok_or_else(|| {
            RunnerError::ResumableState(format!("upload journal lost output {:?}", output.name))
        })?;
    *stored = output.clone();
    persist_upload_journal(path, journal)
}

fn remove_upload_journal(path: &Path) -> Result<(), RunnerError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RunnerError::Workspace(format!(
            "failed to remove completed upload journal: {error}"
        ))),
    }
}

fn validate_output_names(outputs: &[OutputDeclaration]) -> Result<(), RunnerError> {
    let mut names = HashSet::with_capacity(outputs.len());
    for declaration in outputs {
        let bytes = declaration.name.as_bytes();
        let has_valid_length = (1..=128).contains(&bytes.len());
        let has_valid_start = bytes.first().is_some_and(u8::is_ascii_alphanumeric);
        let has_valid_characters = bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));

        if !has_valid_length || !has_valid_start || !has_valid_characters {
            return Err(RunnerError::Output(format!(
                "output name {:?} must be 1..=128 ASCII bytes, start with an alphanumeric character, and contain only alphanumerics, '.', '_', or '-'",
                declaration.name
            )));
        }
        if !names.insert(declaration.name.as_str()) {
            return Err(RunnerError::Output(format!(
                "duplicate output name {:?}",
                declaration.name
            )));
        }
    }
    Ok(())
}

fn validate_data_classification(classification: &str) -> Result<&str, RunnerError> {
    match classification {
        "public" | "internal" | "confidential" | "secret" => Ok(classification),
        _ => Err(RunnerError::Output(format!(
            "unknown output data classification {classification:?}"
        ))),
    }
}

fn validate_output_path(path: &str) -> Result<PathBuf, RunnerError> {
    if path.is_empty()
        || path
            .split(['/', '\\'])
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(RunnerError::Output(format!(
            "output path `{path}` contains an empty, current, or parent component"
        )));
    }

    let path = Path::new(path);
    if path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir | Component::CurDir
        )
    }) {
        return Err(RunnerError::Output(format!(
            "output path `{}` must be a normalized relative path",
            path.display()
        )));
    }

    Ok(path.to_path_buf())
}

fn read_declared_output(
    workspace: &WorkspaceGuard,
    relative: &Path,
    limit: u64,
) -> io::Result<Option<Vec<u8>>> {
    match open_output_file(&workspace.output_dir, relative) {
        Ok(file) => read_capped_regular_file(file, limit).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match open_output_file(&workspace.source_dir, relative) {
                Ok(file) => read_capped_regular_file(file, limit).map(Some),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn read_capped_regular_file(mut file: File, limit: u64) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "declared output is not a regular file",
        ));
    }
    if metadata.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "declared output is {} bytes, exceeding the {limit}-byte limit",
                metadata.len()
            ),
        ));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("declared output grew beyond the {limit}-byte limit while being read"),
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_output_file(root: &Path, relative: &Path) -> io::Result<File> {
    use rustix::fs::{open, openat, Mode, OFlags};

    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output path is not a normalized relative path",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let Some((leaf, parents)) = components.split_last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output path names no file",
        ));
    };

    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut directory = open(root, directory_flags, Mode::empty()).map_err(io::Error::from)?;
    for parent in parents {
        directory =
            openat(&directory, *parent, directory_flags, Mode::empty()).map_err(io::Error::from)?;
    }
    let file = openat(
        &directory,
        *leaf,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(file))
}

#[cfg(not(unix))]
fn open_output_file(root: &Path, relative: &Path) -> io::Result<File> {
    let canonical_root = std::fs::canonicalize(root)?;
    let candidate = root.join(relative);
    let canonical_candidate = std::fs::canonicalize(&candidate)?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "declared output escapes the workspace",
        ));
    }
    File::open(canonical_candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Duration as ChronoDuration;
    use tempfile::TempDir;

    use crate::backend::process::ProcessSandboxBackend;
    use crate::client::{InMemoryControlPlane, InMemoryObjectStore};
    use crate::identity::RunnerIdentity;
    use crate::types::{JobSpec, OutputDeclaration, ResourceSpec, SandboxSpec, WorkspaceLayout};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn output(path: &str, required: bool) -> OutputDeclaration {
        output_named("artifact", path, required)
    }

    fn output_named(name: &str, path: &str, required: bool) -> OutputDeclaration {
        OutputDeclaration {
            name: name.to_string(),
            path: path.to_string(),
            media_type: "application/octet-stream".to_string(),
            required,
        }
    }

    fn claim_with_outputs(outputs: Vec<OutputDeclaration>) -> JobClaim {
        let job_spec = JobSpec {
            argv: vec!["/bin/true".to_string()],
            env: Default::default(),
            working_directory: None,
            workspace_layout: WorkspaceLayout::default(),
            input_manifest_hash: "none".to_string(),
            sandbox: SandboxSpec::default(),
            resources: ResourceSpec::default(),
            outputs,
            max_attempts: 1,
        };
        JobClaim {
            job_id: uuid::Uuid::now_v7(),
            attempt_id: uuid::Uuid::now_v7(),
            attempt_number: 1,
            lease_id: uuid::Uuid::now_v7(),
            lease_generation: 1,
            expires_at: Utc::now() + ChronoDuration::minutes(1),
            job_spec_hash: job_spec.compute_hash(),
            input_manifest_hash: "none".to_string(),
            data_classification: "internal".to_string(),
            job_spec,
            lease_token: "test-lease".to_string(),
            presigned_urls: Default::default(),
        }
    }

    fn test_agent(base: &TempDir, object_store: Arc<dyn ObjectStoreClient>) -> RunnerAgent {
        let organization_id = uuid::Uuid::now_v7();
        RunnerAgent::new(
            RunnerIdentity::generate(organization_id, "test-runner", "process", None),
            Arc::new(InMemoryControlPlane::new()),
            object_store,
            WorkspaceManager::new(base.path()),
            Arc::new(ProcessSandboxBackend::new()),
        )
    }

    #[test]
    fn output_paths_must_be_normalized_and_relative() {
        for path in [
            "/etc/passwd",
            "../secret",
            "nested/../secret",
            "./file",
            "a/./b",
            "a//b",
            "trailing/",
        ] {
            assert!(
                matches!(validate_output_path(path), Err(RunnerError::Output(_))),
                "path must be refused: {path}"
            );
        }
        assert_eq!(
            validate_output_path("nested/artifact.bin").unwrap(),
            PathBuf::from("nested/artifact.bin")
        );
    }

    #[test]
    fn output_names_are_unique_and_strictly_safe() {
        let valid = vec![output_named("Artifact_1.test-x", "artifact", true)];
        validate_output_names(&valid).unwrap();

        let too_long = "a".repeat(129);
        for name in [
            "",
            ".hidden",
            "_hidden",
            "-hidden",
            "with/slash",
            "with space",
            "unicode-é",
            too_long.as_str(),
        ] {
            let declarations = vec![output_named(name, "artifact", true)];
            assert!(
                matches!(
                    validate_output_names(&declarations),
                    Err(RunnerError::Output(_))
                ),
                "name must be refused: {name:?}"
            );
        }

        let duplicate = vec![
            output_named("artifact", "first", true),
            output_named("artifact", "second", true),
        ];
        assert!(matches!(
            validate_output_names(&duplicate),
            Err(RunnerError::Output(message)) if message.contains("duplicate")
        ));
    }

    #[test]
    fn output_classification_is_fail_closed() {
        for classification in ["public", "internal", "confidential", "secret"] {
            assert_eq!(
                validate_data_classification(classification).unwrap(),
                classification
            );
        }
        for classification in ["", "unknown", "top-secret", "PUBLIC"] {
            assert!(matches!(
                validate_data_classification(classification),
                Err(RunnerError::Output(_))
            ));
        }
    }

    #[test]
    fn claim_integrity_rejects_expiry_and_wire_hash_mismatches() {
        let mut claim = claim_with_outputs(vec![]);
        claim.expires_at = Utc::now() - ChronoDuration::seconds(1);
        assert!(matches!(
            validate_claim_integrity(&claim),
            Err(RunnerError::LeaseExpired(_))
        ));

        claim.expires_at = Utc::now() + ChronoDuration::minutes(1);
        claim.job_spec_hash = "sha256:deadbeef".to_string();
        assert!(matches!(
            validate_claim_integrity(&claim),
            Err(RunnerError::Attestation(message)) if message.contains("job_spec_hash")
        ));

        claim.job_spec_hash = claim.job_spec.compute_hash();
        claim.input_manifest_hash = "sha256:different".to_string();
        assert!(matches!(
            validate_claim_integrity(&claim),
            Err(RunnerError::Attestation(message)) if message.contains("input manifest")
        ));
    }

    #[test]
    fn job_spec_hash_is_independent_of_environment_insertion_order() {
        let mut first = claim_with_outputs(vec![]).job_spec;
        first.env.insert("LANG".to_string(), "C".to_string());
        first.env.insert("TZ".to_string(), "UTC".to_string());

        let mut second = first.clone();
        second.env.clear();
        second.env.insert("TZ".to_string(), "UTC".to_string());
        second.env.insert("LANG".to_string(), "C".to_string());

        assert_eq!(first.compute_hash(), second.compute_hash());
    }

    #[cfg(unix)]
    #[test]
    fn output_reader_refuses_symlink_escape() {
        use std::os::unix::fs::symlink;

        let base = TempDir::new().unwrap();
        let manager = WorkspaceManager::new(base.path().join("workspaces"));
        let workspace = manager
            .create_workspace(uuid::Uuid::now_v7(), uuid::Uuid::now_v7())
            .unwrap();
        let outside = base.path().join("host-secret");
        fs::write(&outside, b"must-not-be-read").unwrap();
        symlink(&outside, workspace.output_dir.join("artifact")).unwrap();

        let error = read_declared_output(&workspace, Path::new("artifact"), 1024).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn output_reader_enforces_the_byte_cap() {
        let base = TempDir::new().unwrap();
        let manager = WorkspaceManager::new(base.path());
        let workspace = manager
            .create_workspace(uuid::Uuid::now_v7(), uuid::Uuid::now_v7())
            .unwrap();
        fs::write(workspace.output_dir.join("artifact"), b"too large").unwrap();

        let error = read_declared_output(&workspace, Path::new("artifact"), 3).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn missing_required_output_fails_harvesting_but_optional_output_is_skipped() {
        let base = TempDir::new().unwrap();
        let agent = test_agent(&base, Arc::new(InMemoryObjectStore::new()));
        let mut workspace = agent
            .workspace_manager
            .create_workspace(uuid::Uuid::now_v7(), uuid::Uuid::now_v7())
            .unwrap();

        let required = claim_with_outputs(vec![output("missing", true)]);
        assert!(matches!(
            agent.harvest_outputs(&required, &workspace, 1, None).await,
            Err(RunnerError::Output(message)) if message.contains("required output")
        ));

        let optional = claim_with_outputs(vec![output("missing", false)]);
        assert!(agent
            .harvest_outputs(&optional, &workspace, 1, None)
            .await
            .unwrap()
            .is_empty());
        workspace.teardown().unwrap();
    }

    #[derive(Default)]
    struct FailingUploadStore;

    #[async_trait]
    impl ObjectStoreClient for FailingUploadStore {
        async fn fetch_input_bundle(&self, _hash: &str) -> Result<Vec<u8>, RunnerError> {
            Err(RunnerError::ObjectStore("unused in test".to_string()))
        }

        async fn upload_artifact(&self, _key: &str, _data: &[u8]) -> Result<(), RunnerError> {
            Err(RunnerError::ObjectStore(
                "injected upload failure".to_string(),
            ))
        }
    }

    struct FlakyUploadStore {
        failures_remaining: AtomicUsize,
        attempts: AtomicUsize,
    }

    struct WaitForLeaseCancellation;

    #[async_trait]
    impl RunnerBackend for WaitForLeaseCancellation {
        fn name(&self) -> &'static str {
            "wait-for-lease-cancellation"
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn execute(
            &self,
            _job: &JobSpec,
            _workspace: &WorkspaceGuard,
            mut cancel_rx: watch::Receiver<bool>,
        ) -> Result<crate::backend::ExecutionOutcome, RunnerError> {
            if !*cancel_rx.borrow() {
                cancel_rx.changed().await.map_err(|_| {
                    RunnerError::Cancelled("cancellation channel closed".to_string())
                })?;
            }
            if *cancel_rx.borrow() {
                Err(RunnerError::Cancelled("lease deadline reached".to_string()))
            } else {
                panic!("backend woke without cancellation")
            }
        }
    }

    #[tokio::test]
    async fn absolute_claim_expiry_cancels_execution_before_the_job_wall_clock() {
        let base = TempDir::new().unwrap();
        let organization_id = uuid::Uuid::now_v7();
        let identity = RunnerIdentity::generate(organization_id, "test-runner", "process", None);
        let control_plane = Arc::new(InMemoryControlPlane::new());
        control_plane.register(&identity).await.unwrap();

        let job_spec = claim_with_outputs(vec![]).job_spec;
        control_plane
            .queue_job(
                uuid::Uuid::now_v7(),
                organization_id,
                uuid::Uuid::now_v7(),
                job_spec,
            )
            .await;
        let mut claim = control_plane
            .claim(&ClaimRequest {
                runner_id: identity.id,
                organization_id,
                capabilities: identity.capabilities.clone(),
                max_concurrency: 1,
            })
            .await
            .unwrap()
            .unwrap();
        claim.expires_at = Utc::now() + ChronoDuration::milliseconds(75);

        let agent = RunnerAgent::new(
            identity,
            control_plane.clone(),
            Arc::new(InMemoryObjectStore::new()),
            WorkspaceManager::new(base.path()),
            Arc::new(WaitForLeaseCancellation),
        );
        tokio::time::timeout(Duration::from_secs(2), agent.execute_claim(claim))
            .await
            .expect("lease deadline must interrupt execution")
            .unwrap();

        let attestations = control_plane.get_attestations().await;
        assert_eq!(attestations.len(), 1);
        let statement: AttestationStatement =
            serde_json::from_slice(&attestations[0].statement).unwrap();
        assert_eq!(statement.result, "cancelled");
    }

    #[async_trait]
    impl ObjectStoreClient for FlakyUploadStore {
        async fn fetch_input_bundle(&self, _hash: &str) -> Result<Vec<u8>, RunnerError> {
            Err(RunnerError::ObjectStore("unused in test".to_string()))
        }

        async fn upload_artifact(&self, _key: &str, _data: &[u8]) -> Result<(), RunnerError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self
                .failures_remaining
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                Err(RunnerError::ObjectStore("temporary outage".to_string()))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn artifact_upload_failure_is_not_silently_attested() {
        let base = TempDir::new().unwrap();
        let agent = test_agent(&base, Arc::new(FailingUploadStore));
        let workspace = agent
            .workspace_manager
            .create_workspace(uuid::Uuid::now_v7(), uuid::Uuid::now_v7())
            .unwrap();
        fs::write(workspace.output_dir.join("artifact"), b"artifact bytes").unwrap();
        let claim = claim_with_outputs(vec![output("artifact", true)]);

        assert!(matches!(
            agent.harvest_outputs(&claim, &workspace, 1, None).await,
            Err(RunnerError::TransientOutput(message)) if message.contains("injected upload failure")
        ));
    }

    #[tokio::test]
    async fn every_output_name_is_validated_before_the_first_upload() {
        let base = TempDir::new().unwrap();
        let agent = test_agent(&base, Arc::new(FailingUploadStore));
        let workspace = agent
            .workspace_manager
            .create_workspace(uuid::Uuid::now_v7(), uuid::Uuid::now_v7())
            .unwrap();
        fs::write(workspace.output_dir.join("first"), b"artifact bytes").unwrap();
        let claim = claim_with_outputs(vec![
            output_named("first", "first", true),
            output_named(".invalid", "second", false),
        ]);

        assert!(matches!(
            agent.harvest_outputs(&claim, &workspace, 1, None).await,
            Err(RunnerError::Output(message))
                if message.contains("output name") && !message.contains("injected upload failure")
        ));
    }

    #[tokio::test]
    async fn durable_journal_resumes_upload_without_reexecuting_output_preparation() {
        let base = TempDir::new().unwrap();
        let store = Arc::new(FlakyUploadStore {
            failures_remaining: AtomicUsize::new(3),
            attempts: AtomicUsize::new(0),
        });
        let agent = test_agent(&base, store.clone());
        let workspace = agent
            .workspace_manager
            .create_workspace(uuid::Uuid::now_v7(), uuid::Uuid::now_v7())
            .unwrap();
        fs::write(workspace.output_dir.join("artifact"), b"stable bytes").unwrap();
        let claim = claim_with_outputs(vec![output("artifact", true)]);
        let journal_path = agent
            .workspace_manager
            .upload_journal_path(claim.job_id, claim.attempt_id);
        let mut journal = UploadJournal {
            version: 1,
            job_id: claim.job_id,
            attempt_id: claim.attempt_id,
            job_spec_hash: claim.job_spec_hash.clone(),
            started_at: Utc::now().to_rfc3339(),
            exit_code: 0,
            outputs: vec![],
        };
        persist_upload_journal(&journal_path, &journal).unwrap();

        assert!(matches!(
            agent
                .harvest_outputs(
                    &claim,
                    &workspace,
                    1,
                    Some((&mut journal, &journal_path))
                )
                .await,
            Err(RunnerError::TransientOutput(message)) if message.contains("temporary outage")
        ));
        drop(journal);

        let mut resumed = load_upload_journal(&journal_path, &claim).unwrap().unwrap();
        let outputs = agent
            .harvest_outputs(&claim, &workspace, 1, Some((&mut resumed, &journal_path)))
            .await
            .unwrap();

        assert_eq!(outputs.len(), 1);
        assert_eq!(store.attempts.load(Ordering::SeqCst), 4);
        assert!(resumed.outputs[0].object_stored);
        assert!(resumed.outputs[0].registered);
    }

    #[tokio::test]
    async fn aggregate_output_bytes_cannot_exceed_the_job_limit() {
        let base = TempDir::new().unwrap();
        let agent = test_agent(&base, Arc::new(InMemoryObjectStore::new()));
        let workspace = agent
            .workspace_manager
            .create_workspace(uuid::Uuid::now_v7(), uuid::Uuid::now_v7())
            .unwrap();
        let chunk = vec![b'x'; 600 * 1024];
        fs::write(workspace.output_dir.join("first"), &chunk).unwrap();
        fs::write(workspace.output_dir.join("second"), &chunk).unwrap();
        let claim = claim_with_outputs(vec![
            output_named("first", "first", true),
            output_named("second", "second", true),
        ]);

        assert!(matches!(
            agent.harvest_outputs(&claim, &workspace, 1, None).await,
            Err(RunnerError::Output(message)) if message.contains("exceeding")
        ));
    }

    #[tokio::test]
    async fn harvest_preserves_claim_classification() {
        let base = TempDir::new().unwrap();
        let control_plane = Arc::new(InMemoryControlPlane::new());
        let agent = RunnerAgent::new(
            RunnerIdentity::generate(uuid::Uuid::now_v7(), "test-runner", "process", None),
            control_plane.clone(),
            Arc::new(InMemoryObjectStore::new()),
            WorkspaceManager::new(base.path()),
            Arc::new(ProcessSandboxBackend::new()),
        );
        let workspace = agent
            .workspace_manager
            .create_workspace(uuid::Uuid::now_v7(), uuid::Uuid::now_v7())
            .unwrap();
        fs::write(workspace.output_dir.join("artifact"), b"artifact bytes").unwrap();
        let mut claim = claim_with_outputs(vec![output("artifact", true)]);
        claim.data_classification = "confidential".to_string();

        agent
            .harvest_outputs(&claim, &workspace, 1, None)
            .await
            .unwrap();

        let outputs = control_plane.get_outputs().await;
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].classification, "confidential");

        claim.data_classification = "unknown".to_string();
        assert!(matches!(
            agent.harvest_outputs(&claim, &workspace, 1, None).await,
            Err(RunnerError::Output(message)) if message.contains("unknown output data classification")
        ));
        assert_eq!(control_plane.get_outputs().await.len(), 1);
    }
}
