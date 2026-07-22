//! Blackboard transport: per-run artifact fan-out + the board-read seam
//! (Phase 5 STEP 5.3 client surface).
//!
//! Two pieces live here, both daemon-owned but neither depending on the workflow
//! crate (the daemon must not — `codypendent-workflow` owns the authoritative
//! `BlackboardStore`, and depending on it would invert the layering the executor
//! seam exists to avoid):
//!
//! * [`BlackboardHub`] — a per-*workflow-run* [`tokio::sync::broadcast`] fan-out,
//!   mirroring [`crate::documents::DocumentHub`] but keyed by the durable
//!   workflow-run id and carrying a [`BlackboardItemView`]. The workflow executor
//!   publishes each posted (or superseded) artifact here as it lands, and the
//!   server subscribes a client's `Subscription::Blackboard` forwarder to it. It is
//!   **not** a source of truth — the workflow crate's `BlackboardStore` is; a
//!   missed delivery is harmless, because a subscriber's baseline comes from the
//!   read command and each item is merged idempotently by id.
//!
//! * [`BlackboardReader`] — the dependency-inversion seam for *reading* a run's
//!   board. The daemon declares what it needs (project a run's board, kind-filtered,
//!   into [`BlackboardItemView`]s); the `codypendentd` assembly implements it over
//!   `codypendent-workflow`'s `BlackboardStore` on the daemon's pool, exactly as it
//!   implements [`WorkflowStarter`](crate::workflows::WorkflowStarter) and
//!   [`DocumentMutator`](crate::documents::DocumentMutator). Like the document
//!   seams it is **request/reply**: the server awaits the projected items so it can
//!   reply `BlackboardItems`, so the method returns a boxed future (no `async-trait`
//!   dependency). The default-`None`
//!   [`RunExecutor::blackboard_reader`](crate::executor::RunExecutor::blackboard_reader)
//!   leaves it unwired — the lib-only / test server then rejects `ReadBlackboard`
//!   with `workflow.transport-unavailable`, exactly as `StartWorkflow` is without a
//!   starter.
//!
//! There is deliberately **no** post seam here: only the workflow executor writes
//! the board (an agent posts through the runtime tool layer), so no client-facing
//! post command exists. An Observer may read; nobody posts over the wire.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use codypendent_protocol::{BlackboardItemView, ClientId, CodypendentError};
use tokio::sync::broadcast;

/// Per-run channel depth. A run's board advances far slower than a session's
/// event stream (a handful of typed artifacts per node, not a streaming token
/// feed), so a modest buffer bounds memory; a receiver that still falls behind is
/// signalled `Lagged` and simply re-reads the board (idempotent merge by id),
/// never stalling the publisher.
const CHANNEL_CAPACITY: usize = 256;

/// An in-memory, per-workflow-run blackboard fan-out shared by every clone (an
/// [`Arc`]), so the executor's post path (publisher) and each `Blackboard`
/// forwarder (subscriber) see the same channels.
#[derive(Debug, Clone, Default)]
pub struct BlackboardHub {
    channels: Arc<Mutex<HashMap<String, broadcast::Sender<BlackboardItemView>>>>,
}

impl BlackboardHub {
    /// An empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a run's live board stream, creating the channel lazily if this
    /// is the first subscriber. The returned receiver observes only items published
    /// *after* this call — a subscriber gets its baseline from the read command,
    /// then converges via the live stream (merges are idempotent by id, so a small
    /// overlap or gap self-heals).
    pub fn subscribe(
        &self,
        workflow_run_id: impl Into<String>,
    ) -> broadcast::Receiver<BlackboardItemView> {
        self.lock()
            .entry(workflow_run_id.into())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publish one posted (or superseded) artifact to a run's subscribers.
    /// Best-effort: no channel (no subscribers ever) or all receivers dropped
    /// discards it silently — the workflow `BlackboardStore` remains the durable
    /// record.
    pub fn publish(&self, workflow_run_id: &str, item: BlackboardItemView) {
        if let Some(sender) = self.lock().get(workflow_run_id) {
            let _ = sender.send(item);
        }
    }

    /// Number of runs with a live channel (subscribed at least once).
    #[must_use]
    pub fn run_count(&self) -> usize {
        self.lock().len()
    }

    /// Drop channels whose last receiver has detached, so a long-lived daemon's hub
    /// does not retain one channel per workflow run ever subscribed.
    pub fn prune_idle(&self) {
        self.lock().retain(|_, sender| sender.receiver_count() > 0);
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, broadcast::Sender<BlackboardItemView>>> {
        // Held only for map lookups/inserts, never across an await, so poisoning
        // indicates a bug elsewhere; surface it loudly.
        self.channels.lock().expect("blackboard hub mutex poisoned")
    }
}

/// A client's request to read a workflow run's blackboard.
#[derive(Debug, Clone)]
pub struct ReadBlackboardRequest {
    /// The durable workflow-run id whose board to read.
    pub workflow_run_id: String,
    /// A blackboard artifact kind to filter by, or all kinds when `None`.
    pub kind: Option<String>,
    /// Include superseded revisions too; `false` returns only the live board.
    pub include_superseded: bool,
    /// The identity of the reading client (for attribution / audit).
    pub client_id: ClientId,
}

/// The future a [`BlackboardReader`] returns: the projected board items to reply
/// with, or a structured [`CodypendentError`] the server rejects with. Boxed so
/// the trait stays object-safe without an `async-trait` dependency (matching the
/// [`DocumentMutator`](crate::documents::DocumentMutator) seam).
pub type BlackboardReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<BlackboardItemView>, CodypendentError>> + Send + 'a>>;

/// The daemon's seam for *reading* a durable run's blackboard from an accepted
/// `ReadBlackboard` command.
///
/// Implemented by the assembly over `codypendent-workflow`'s `BlackboardStore`
/// (`query`, kind-filtered) on the daemon's pool, and injected alongside the
/// [`RunExecutor`](crate::executor::RunExecutor). The default-`None`
/// [`RunExecutor::blackboard_reader`](crate::executor::RunExecutor::blackboard_reader)
/// leaves it unwired — the lib-only / test server then rejects `ReadBlackboard`
/// with `workflow.transport-unavailable`.
pub trait BlackboardReader: Send + Sync {
    /// Project `request`'s run board (kind-filtered) into
    /// [`BlackboardItemView`]s. A store failure is surfaced verbatim to the client
    /// as a `CommandRejected`; an unknown run yields an empty board (its own board
    /// is simply empty), never an error.
    fn read(&self, request: ReadBlackboardRequest) -> BlackboardReadFuture<'_>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(workflow_run_id: &str, id: &str, kind: &str) -> BlackboardItemView {
        BlackboardItemView {
            id: id.to_string(),
            workflow_run_id: workflow_run_id.to_string(),
            kind: kind.to_string(),
            payload: json!({ "note": id }),
            author: json!({ "node_id": "n1" }),
            confidence: None,
            evidence: Vec::new(),
            revision: 1,
            superseded_by: None,
        }
    }

    #[tokio::test]
    async fn subscriber_receives_published_items_in_order() {
        let hub = BlackboardHub::new();
        let run = "wfrun-1";
        let mut rx = hub.subscribe(run);

        hub.publish(run, item(run, "a", "finding"));
        hub.publish(run, item(run, "b", "decision"));

        assert_eq!(rx.recv().await.unwrap().id, "a");
        assert_eq!(rx.recv().await.unwrap().id, "b");
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_a_silent_noop() {
        let hub = BlackboardHub::new();
        hub.publish("wfrun-void", item("wfrun-void", "a", "finding"));
        assert_eq!(hub.run_count(), 0);
    }

    #[tokio::test]
    async fn channels_are_isolated_per_run() {
        let hub = BlackboardHub::new();
        let mut rx_a = hub.subscribe("wfrun-a");
        let _rx_b = hub.subscribe("wfrun-b");

        hub.publish("wfrun-a", item("wfrun-a", "x", "finding"));
        assert_eq!(rx_a.recv().await.unwrap().id, "x");
        assert_eq!(hub.run_count(), 2);
    }

    #[test]
    fn prune_drops_channels_whose_receivers_detached() {
        let hub = BlackboardHub::new();
        {
            let _rx = hub.subscribe("wfrun-1");
            assert_eq!(hub.run_count(), 1);
        }
        hub.prune_idle();
        assert_eq!(hub.run_count(), 0);
    }
}
