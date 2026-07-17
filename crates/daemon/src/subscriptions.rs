//! Per-session event fan-out to subscribed clients (STEP 1.3 / STEP 1.11).
//!
//! The command write path publishes each *persisted* event here (persist before
//! publish — RULE 2); attached clients receive them live. This is an in-memory
//! broadcast only: it is never the source of truth (the ledger is), so a client
//! that misses events simply re-attaches and catches up from its last sequence.
//!
//! Each session gets its own [`tokio::sync::broadcast`] channel, created lazily
//! on first [`subscribe`](SubscriptionHub::subscribe). [`publish`] never blocks
//! and never errors the caller: a broadcast `send` only fails when there are no
//! live receivers (publishing to zero subscribers is normal — a run holds no
//! client handles, exit criterion 1), and a slow receiver is signalled `Lagged`
//! on *its* end and falls back to re-attach — it never stalls the writer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use codypendent_protocol::{SessionEvent, SessionId};
use tokio::sync::broadcast;

/// Per-session channel depth. Generous enough that a briefly-busy client is not
/// dropped, small enough to bound memory; a client that falls further behind is
/// marked `Lagged` and re-attaches rather than the writer blocking.
const CHANNEL_CAPACITY: usize = 1024;

/// An in-memory, per-session event fan-out shared by every clone (an [`Arc`]),
/// so the command processor, the protocol server, and each connection task all
/// see the same channels.
#[derive(Debug, Clone, Default)]
pub struct SubscriptionHub {
    channels: Arc<Mutex<HashMap<SessionId, broadcast::Sender<SessionEvent>>>>,
}

impl SubscriptionHub {
    /// An empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a session's live event stream, creating the channel lazily
    /// if this is the first subscriber. The returned receiver observes only
    /// events published *after* this call.
    pub fn subscribe(&self, session_id: SessionId) -> broadcast::Receiver<SessionEvent> {
        let mut channels = self.lock();
        channels
            .entry(session_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publish one persisted event to a session's subscribers. Best-effort: if
    /// no channel exists (no subscribers ever) or all receivers have dropped,
    /// the event is discarded silently — the ledger remains the durable record.
    pub fn publish(&self, session_id: SessionId, event: SessionEvent) {
        let channels = self.lock();
        if let Some(sender) = channels.get(&session_id) {
            // Ignore the count/`SendError`: a full channel drops the oldest for
            // slow receivers (never blocks), and zero receivers is normal.
            let _ = sender.send(event);
        }
    }

    /// Number of sessions with a live channel (subscribed at least once). Useful
    /// for diagnostics and tests.
    pub fn session_count(&self) -> usize {
        self.lock().len()
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<SessionId, broadcast::Sender<SessionEvent>>> {
        // The lock is only ever held for map lookups/inserts (never across an
        // await or a blocking call), so poisoning indicates a bug elsewhere; we
        // surface it loudly rather than mask it.
        self.channels
            .lock()
            .expect("subscription hub mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use codypendent_protocol::{Actor, EventBody};

    fn event(sequence: u64, text: &str) -> SessionEvent {
        SessionEvent {
            sequence,
            occurred_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: EventBody::NoteAppended {
                text: text.to_string(),
                run_id: None,
            },
        }
    }

    #[tokio::test]
    async fn subscriber_receives_published_events_in_order() {
        let hub = SubscriptionHub::new();
        let session = SessionId::new();
        let mut rx = hub.subscribe(session);

        hub.publish(session, event(1, "first"));
        hub.publish(session, event(2, "second"));

        assert_eq!(rx.recv().await.unwrap().sequence, 1);
        assert_eq!(rx.recv().await.unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_a_silent_noop() {
        let hub = SubscriptionHub::new();
        // No channel exists for this session; publishing must not panic.
        hub.publish(SessionId::new(), event(1, "into the void"));
        assert_eq!(hub.session_count(), 0);
    }

    #[tokio::test]
    async fn channels_are_isolated_per_session() {
        let hub = SubscriptionHub::new();
        let a = SessionId::new();
        let b = SessionId::new();
        let mut rx_a = hub.subscribe(a);
        let _rx_b = hub.subscribe(b);

        hub.publish(a, event(7, "for a"));
        // b's stream saw nothing; a's got exactly the one event.
        assert_eq!(rx_a.recv().await.unwrap().sequence, 7);
        assert_eq!(hub.session_count(), 2);
    }

    #[test]
    fn hub_clone_shares_channels() {
        let hub = SubscriptionHub::new();
        let session = SessionId::new();
        let _rx = hub.subscribe(session);
        let clone = hub.clone();
        // The clone observes the channel created through the original.
        assert_eq!(clone.session_count(), 1);
    }
}
