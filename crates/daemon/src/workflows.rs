//! Workflow-start seam (dependency inversion), Phase 5 STEP 5.2.
//!
//! A `StartWorkflow` command creates a durable workflow run that lives in its own
//! store *outside* the session ledger — so, like `MutateDocument`, it is
//! intercepted at the connection level and applied through a seam the daemon
//! declares and the `codypendentd` assembly fills (only the assembly can name
//! `codypendent-workflow` and reach the pool). The default-`None`
//! [`RunExecutor::workflow_starter`](crate::executor::RunExecutor::workflow_starter)
//! leaves it unwired — the lib-only / test server then rejects `StartWorkflow`
//! with `workflow.transport-unavailable`, exactly as an executor-less run stays
//! `Queued`.

use std::future::Future;
use std::pin::Pin;

use codypendent_protocol::{ClientId, CodypendentError};
use serde_json::Value;

/// A client's request to start a durable workflow run from a manifest.
#[derive(Debug, Clone)]
pub struct StartWorkflowRequest {
    /// The workflow manifest YAML (its content, never a path — the daemon does not
    /// read an arbitrary client-named file).
    pub manifest: String,
    /// The typed inputs the manifest declares (opaque JSON to the daemon; the
    /// store records them with the run).
    pub inputs: Value,
    /// The command's idempotency key: a duplicate `StartWorkflow` delivery (a
    /// client retrying after a lost acknowledgement) carries the same key, so the
    /// seam creates the run idempotently — the same key resolves to the same run
    /// rather than a second one.
    pub idempotency_key: String,
    /// The identity of the starting client, for attribution.
    pub client_id: ClientId,
}

/// The future a [`WorkflowStarter`] returns: the new durable workflow-run id to
/// reply with, or a structured [`CodypendentError`] the server rejects with. Boxed
/// so the trait stays object-safe without an `async-trait` dependency (matching
/// the [`DocumentMutator`](crate::documents::DocumentMutator) seam).
pub type WorkflowStartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, CodypendentError>> + Send + 'a>>;

/// The daemon's seam for *creating* a durable run from an accepted `StartWorkflow`.
///
/// Implemented by the assembly over `codypendent-workflow` — compile the manifest,
/// then `WorkflowStore::create_run` on the daemon's pool — and injected alongside
/// the [`RunExecutor`](crate::executor::RunExecutor). Driving the created run is a
/// later step; this seam only makes runs durably creatable, so a client (or a
/// future recovery pass) has a run row to advance.
pub trait WorkflowStarter: Send + Sync {
    /// Compile `request`'s manifest and create a durable run, returning its id. A
    /// manifest that does not compile (or a store failure) is surfaced verbatim to
    /// the client as a `CommandRejected`; nothing is created.
    fn start(&self, request: StartWorkflowRequest) -> WorkflowStartFuture<'_>;
}
