//! Workflow observability transport: per-run node-lifecycle fan-out + the
//! run-snapshot seam (Phase 5 STEP 5.2 / T9 client surface).
//!
//! Two pieces live here, both daemon-owned but neither depending on the workflow
//! crate (the daemon must not — `codypendent-workflow` owns the authoritative
//! durable store, and depending on it would invert the layering the executor seam
//! exists to avoid). This mirrors [`crate::blackboard`] exactly, one layer up: the
//! blackboard streams an agent's posted artifacts, this streams the driver's node
//! transitions.
//!
//! * [`WorkflowHub`] — a per-*workflow-run* [`tokio::sync::broadcast`] fan-out,
//!   mirroring [`crate::blackboard::BlackboardHub`] but carrying a
//!   [`WorkflowEvent`]. The workflow host publishes each node transition (and
//!   run-phase change) here as the driver records it, and the server subscribes a
//!   client's `Subscription::Workflow` forwarder to it. It is **not** a source of
//!   truth — the workflow crate's store is; a missed delivery is harmless, because a
//!   subscriber's baseline comes from the snapshot command and each node transition
//!   is full-state (merged idempotently by `node_id`).
//!
//! * [`WorkflowReader`] — the dependency-inversion seam for *reading* a run's
//!   observability snapshot. The daemon declares what it needs (project a run's phase
//!   and every node's view into a [`WorkflowRunSnapshot`]); the `codypendentd` assembly
//!   implements it over the workflow store on the daemon's pool, exactly as it
//!   implements [`BlackboardReader`](crate::blackboard::BlackboardReader). Like the
//!   blackboard seam it is **request/reply**: the server awaits the projected
//!   snapshot so it can reply `WorkflowRunSnapshot`, so the method returns a boxed
//!   future (no `async-trait` dependency). The default-`None`
//!   [`RunExecutor::workflow_reader`](crate::executor::RunExecutor::workflow_reader)
//!   leaves it unwired — the lib-only / test server then rejects `ReadWorkflowRun`
//!   with `workflow.transport-unavailable`, exactly as `StartWorkflow` is without a
//!   starter.
//!
//! The **catch-up / idempotency contract** (why no watermark is needed): a
//! subscriber attaches its `Subscription::Workflow` forwarder first, then issues
//! `ReadWorkflowRun` for the baseline. Because the host publishes each transition
//! **after** persisting it (persist-before-publish), the snapshot read after
//! subscribing already reflects every transition committed before the read, and any
//! transition in the channel is therefore either also in the snapshot (a harmless
//! idempotent re-write by `node_id`) or strictly newer (a correct advance). So a
//! subscriber applies the snapshot as its baseline and folds live transitions on
//! top — no gaps, and duplicates are inert.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use codypendent_protocol::{ClientId, CodypendentError, WorkflowEvent, WorkflowRunSnapshot};
use tokio::sync::broadcast;

/// Per-run channel depth. A run's node graph advances far slower than a session's
/// event stream (a handful of node transitions, not a streaming token feed), so a
/// modest buffer bounds memory; a receiver that still falls behind is signalled
/// `Lagged` and simply re-reads the snapshot (idempotent merge by `node_id`), never
/// stalling the publisher.
const CHANNEL_CAPACITY: usize = 256;

/// An in-memory, per-workflow-run node-lifecycle fan-out shared by every clone (an
/// [`Arc`]), so the host's publish path (publisher) and each `Workflow` forwarder
/// (subscriber) see the same channels.
#[derive(Debug, Clone, Default)]
pub struct WorkflowHub {
    channels: Arc<Mutex<HashMap<String, broadcast::Sender<WorkflowEvent>>>>,
}

impl WorkflowHub {
    /// An empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a run's live node-lifecycle stream, creating the channel lazily
    /// if this is the first subscriber. The returned receiver observes only events
    /// published *after* this call — a subscriber gets its baseline from the snapshot
    /// command, then converges via the live stream (each transition is full-state, so
    /// a small overlap self-heals).
    pub fn subscribe(
        &self,
        workflow_run_id: impl Into<String>,
    ) -> broadcast::Receiver<WorkflowEvent> {
        self.lock()
            .entry(workflow_run_id.into())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publish one node transition (or run-phase change) to a run's subscribers.
    /// Best-effort: no channel (no subscribers ever) or all receivers dropped
    /// discards it silently — the workflow store remains the durable record.
    pub fn publish(&self, workflow_run_id: &str, event: WorkflowEvent) {
        if let Some(sender) = self.lock().get(workflow_run_id) {
            let _ = sender.send(event);
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

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, broadcast::Sender<WorkflowEvent>>> {
        // Held only for map lookups/inserts, never across an await, so poisoning
        // indicates a bug elsewhere; surface it loudly.
        self.channels.lock().expect("workflow hub mutex poisoned")
    }
}

/// A client's request to read a workflow run's observability snapshot.
#[derive(Debug, Clone)]
pub struct ReadWorkflowRunRequest {
    /// The durable workflow-run id whose snapshot to read.
    pub workflow_run_id: String,
    /// The identity of the reading client (for attribution / audit).
    pub client_id: ClientId,
}

/// The future a [`WorkflowReader`] returns: the projected run snapshot to reply
/// with, or a structured [`CodypendentError`] the server rejects with. Boxed so the
/// trait stays object-safe without an `async-trait` dependency (matching the
/// [`BlackboardReader`](crate::blackboard::BlackboardReader) seam).
pub type WorkflowReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WorkflowRunSnapshot, CodypendentError>> + Send + 'a>>;

/// The daemon's seam for *reading* a durable run's observability snapshot from an
/// accepted `ReadWorkflowRun` command.
///
/// Implemented by the assembly over the workflow store (`snapshot`) on the daemon's
/// pool, and injected alongside the [`RunExecutor`](crate::executor::RunExecutor).
/// The default-`None`
/// [`RunExecutor::workflow_reader`](crate::executor::RunExecutor::workflow_reader)
/// leaves it unwired — the lib-only / test server then rejects `ReadWorkflowRun` with
/// `workflow.transport-unavailable`.
pub trait WorkflowReader: Send + Sync {
    /// Project `request`'s run into a [`WorkflowRunSnapshot`]. A store failure is
    /// surfaced verbatim to the client as a `CommandRejected`; an unknown run is a
    /// `workflow.run-not-found` rejection (a run either exists or does not — unlike a
    /// board, whose own board is simply empty).
    fn read(&self, request: ReadWorkflowRunRequest) -> WorkflowReadFuture<'_>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{WorkflowNodeState, WorkflowNodeView, WorkflowRunPhase};

    fn node_event(run: &str, node: &str) -> WorkflowEvent {
        WorkflowEvent::NodeTransitioned(WorkflowNodeView {
            workflow_run_id: run.to_string(),
            node_id: node.to_string(),
            state: WorkflowNodeState::Running,
            attempt: 1,
            cost: None,
            error: None,
            warnings: Vec::new(),
        })
    }

    #[tokio::test]
    async fn subscriber_receives_published_events_in_order() {
        let hub = WorkflowHub::new();
        let run = "wfrun-1";
        let mut rx = hub.subscribe(run);

        hub.publish(run, node_event(run, "a"));
        hub.publish(
            run,
            WorkflowEvent::RunPhaseChanged {
                workflow_run_id: run.to_string(),
                phase: WorkflowRunPhase::Completed,
            },
        );

        match rx.recv().await.unwrap() {
            WorkflowEvent::NodeTransitioned(view) => assert_eq!(view.node_id, "a"),
            other => panic!("expected a node transition, got {other:?}"),
        }
        match rx.recv().await.unwrap() {
            WorkflowEvent::RunPhaseChanged { phase, .. } => {
                assert_eq!(phase, WorkflowRunPhase::Completed)
            }
            other => panic!("expected a run-phase change, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_a_silent_noop() {
        let hub = WorkflowHub::new();
        hub.publish("wfrun-void", node_event("wfrun-void", "a"));
        assert_eq!(hub.run_count(), 0);
    }

    #[tokio::test]
    async fn channels_are_isolated_per_run() {
        let hub = WorkflowHub::new();
        let mut rx_a = hub.subscribe("wfrun-a");
        let _rx_b = hub.subscribe("wfrun-b");

        hub.publish("wfrun-a", node_event("wfrun-a", "x"));
        match rx_a.recv().await.unwrap() {
            WorkflowEvent::NodeTransitioned(view) => assert_eq!(view.node_id, "x"),
            other => panic!("expected a node transition, got {other:?}"),
        }
        assert_eq!(hub.run_count(), 2);
    }

    #[test]
    fn prune_drops_channels_whose_receivers_detached() {
        let hub = WorkflowHub::new();
        {
            let _rx = hub.subscribe("wfrun-1");
            assert_eq!(hub.run_count(), 1);
        }
        hub.prune_idle();
        assert_eq!(hub.run_count(), 0);
    }
}
