//! Process sandbox execution and fail-closed posture tests.
//!
//! Verifies Acceptance Criterion 12:
//! "The runner refuses to execute unconfined. With the enforcing executor unavailable,
//! the job is refused and the lease released; no process is spawned.
//! Test: runner_refuses_job_when_enforcement_unavailable"

use tempfile::TempDir;
use tokio::sync::watch;
use uuid::Uuid;

use codypendent_runner::{
    JobSpec, ProcessSandboxBackend, ResourceSpec, RunnerBackend, RunnerError, SandboxSpec,
    WorkspaceLayout, WorkspaceManager,
};
use codypendent_sandbox::RefusingSandbox;

fn sample_job_spec() -> JobSpec {
    JobSpec {
        argv: vec!["/bin/echo".to_string(), "hello".to_string()],
        env: Default::default(),
        working_directory: None,
        workspace_layout: WorkspaceLayout::default(),
        input_manifest_hash: "none".to_string(),
        sandbox: SandboxSpec::default(),
        resources: ResourceSpec::default(),
        outputs: vec![],
        max_attempts: 1,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_refuses_job_when_enforcement_unavailable() {
    // Injected RefusingSandbox simulating unavailable backend on unsupported host or missing tool
    let refusing_backend = ProcessSandboxBackend::with_executor(Box::new(RefusingSandbox));

    assert!(!refusing_backend.is_available());

    let temp_dir = TempDir::new().unwrap();
    let ws_mgr = WorkspaceManager::new(temp_dir.path());
    let ws = ws_mgr
        .create_workspace(Uuid::now_v7(), Uuid::now_v7())
        .unwrap();

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let job = sample_job_spec();

    let err = refusing_backend
        .execute(&job, &ws, cancel_rx)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, RunnerError::SandboxUnavailable(msg) if msg.contains("refusing to run unconfined")),
        "Expected SandboxUnavailable with 'refusing to run unconfined', got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_resource_caps_are_refused_rather_than_unlimited() {
    let mut job = sample_job_spec();
    job.resources.memory_mb = 0; // Zero memory cap

    let temp_dir = TempDir::new().unwrap();
    let ws_mgr = WorkspaceManager::new(temp_dir.path());
    let ws = ws_mgr
        .create_workspace(Uuid::now_v7(), Uuid::now_v7())
        .unwrap();

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let backend = ProcessSandboxBackend::new();

    if backend.is_available() {
        let err = backend.execute(&job, &ws, cancel_rx).await.unwrap_err();
        assert!(matches!(err, RunnerError::InvalidCommand(_)));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn network_allowlist_fails_closed_without_broker() {
    let mut job = sample_job_spec();
    job.sandbox.network_allowlist = vec!["api.github.com:443".to_string()];

    let temp_dir = TempDir::new().unwrap();
    let ws_mgr = WorkspaceManager::new(temp_dir.path());
    let ws = ws_mgr
        .create_workspace(Uuid::now_v7(), Uuid::now_v7())
        .unwrap();

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let backend = ProcessSandboxBackend::new();

    if backend.is_available() {
        let err = backend.execute(&job, &ws, cancel_rx).await.unwrap_err();
        assert!(
            matches!(err, RunnerError::UnsupportedCapability(msg) if msg.contains("network allowlists require a broker"))
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn process_sandbox_executes_confined_when_available() {
    let backend = ProcessSandboxBackend::new();
    if !backend.is_available() {
        // Skip execution assertion if platform lacks bwrap / seatbelt in test environment
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let ws_mgr = WorkspaceManager::new(temp_dir.path());
    let ws = ws_mgr
        .create_workspace(Uuid::now_v7(), Uuid::now_v7())
        .unwrap();

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let mut job = sample_job_spec();
    // This test exercises process confinement; Linux explicitly refuses to
    // promise subprocess denial for arbitrary native executables.
    job.sandbox.allow_subprocess = true;

    let outcome = backend.execute(&job, &ws, cancel_rx).await.unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert!(String::from_utf8_lossy(&outcome.stdout).contains("hello"));
}
