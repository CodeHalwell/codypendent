//! Collaborative-document transport: per-document CRDT fan-out + the
//! mutation-application seam (Phase 4, STEP 4.3 client transport).
//!
//! Two pieces live here, both daemon-owned but neither depending on the knowledge
//! crate (the daemon must not — knowledge depends on the protocol, and the
//! authoritative Loro document lives in knowledge):
//!
//! * [`DocumentHub`] — a per-*document* [`tokio::sync::broadcast`] fan-out,
//!   mirroring [`crate::subscriptions::SubscriptionHub`] but keyed by
//!   [`DocumentId`] and carrying [`DocumentSync`] rather than session events. The
//!   server publishes a sync here after a mutation applies, and subscribes a
//!   client's `Subscription::Document` forwarder to it. It is **not** a source of
//!   truth: the knowledge CRDT store is. A missed sync is harmless — the CRDT
//!   update is a full idempotent snapshot, so a client re-merges and converges.
//!
//! * [`DocumentMutator`] — the dependency-inversion seam for *applying* a client
//!   mutation. The daemon declares what it needs (map a [`DocumentMutation`] onto
//!   the authoritative document and hand back the [`DocumentSync`] to broadcast);
//!   the `codypendentd` assembly implements it over `codypendent-knowledge`'s
//!   `apply_mutation` + edit-lease enforcement, exactly as it implements
//!   [`RunExecutor`](crate::executor::RunExecutor) over the runtime agent loop.
//!   Unlike the run seam, this one is **request/reply**: the server awaits the
//!   applied sync so it can reply `CommandAccepted`/`CommandRejected` and then
//!   broadcast — so the method returns a boxed future (no `async-trait` dep).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use codypendent_protocol::{ClientId, CodypendentError, DocumentId, DocumentMutation, DocumentSync};
use tokio::sync::broadcast;

/// Per-document channel depth. A document's sync stream is far lower-frequency
/// than a session's event stream (a human editing, not a streaming agent loop),
/// so a smaller buffer than [`crate::subscriptions`] bounds memory; a receiver
/// that still falls behind is signalled `Lagged` and simply re-merges the next
/// snapshot it sees (CRDT convergence), never stalling the publisher.
const CHANNEL_CAPACITY: usize = 256;

/// An in-memory, per-document CRDT-sync fan-out shared by every clone (an
/// [`Arc`]), so the server's mutation path (publisher) and each `Document`
/// forwarder (subscriber) see the same channels.
#[derive(Debug, Clone, Default)]
pub struct DocumentHub {
    channels: Arc<Mutex<HashMap<DocumentId, broadcast::Sender<DocumentSync>>>>,
}

impl DocumentHub {
    /// An empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a document's live sync stream, creating the channel lazily if
    /// this is the first subscriber. The returned receiver observes only syncs
    /// published *after* this call — a subscriber gets its baseline from the
    /// document read path, then converges via the live stream (CRDT merges are
    /// idempotent, so a small overlap or gap self-heals).
    pub fn subscribe(&self, document_id: DocumentId) -> broadcast::Receiver<DocumentSync> {
        self.lock()
            .entry(document_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publish one applied CRDT sync to a document's subscribers. Best-effort: no
    /// channel (no subscribers ever) or all receivers dropped discards it
    /// silently — the knowledge CRDT store remains the durable record.
    pub fn publish(&self, document_id: DocumentId, sync: DocumentSync) {
        if let Some(sender) = self.lock().get(&document_id) {
            let _ = sender.send(sync);
        }
    }

    /// Number of documents with a live channel (subscribed at least once).
    #[must_use]
    pub fn document_count(&self) -> usize {
        self.lock().len()
    }

    /// Drop channels whose last receiver has detached, so a long-lived daemon's
    /// hub does not retain one channel per document ever edited.
    pub fn prune_idle(&self) {
        self.lock().retain(|_, sender| sender.receiver_count() > 0);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<DocumentId, broadcast::Sender<DocumentSync>>>
    {
        // Held only for map lookups/inserts, never across an await, so poisoning
        // indicates a bug elsewhere; surface it loudly.
        self.channels.lock().expect("document hub mutex poisoned")
    }
}

/// A client's request to apply one mutation to a collaborative document. The
/// author is expressed in protocol terms (the mutating [`ClientId`]); the seam
/// maps it to a knowledge `DocumentAuthor` — a `MutateDocument` command is a
/// *human* client edit (an agent authors through the runtime, not this path).
#[derive(Debug, Clone)]
pub struct DocumentMutationRequest {
    pub document_id: DocumentId,
    pub mutation: DocumentMutation,
    /// The identity of the mutating client, for authorship + lease-holder
    /// attribution.
    pub client_id: ClientId,
}

/// The future a [`DocumentMutator`] returns: the applied [`DocumentSync`] to
/// broadcast, or a structured [`CodypendentError`] the server replies with. Boxed
/// so the trait stays object-safe without an `async-trait` dependency.
pub type DocumentMutationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<DocumentSync, CodypendentError>> + Send + 'a>>;

/// The daemon's seam for *applying* an accepted `MutateDocument` command.
///
/// Implemented by the assembly binary over `codypendent-knowledge`'s
/// `apply_mutation` (mode-gated by the document's scope) and its edit-lease
/// `require` (single-writer enforcement); injected into the server alongside the
/// [`RunExecutor`](crate::executor::RunExecutor). The default-`None`
/// [`RunExecutor::document_mutator`](crate::executor::RunExecutor::document_mutator)
/// leaves this unwired — the lib-only / test server then rejects `MutateDocument`
/// with `document.transport-unavailable`, exactly as before this seam existed.
pub trait DocumentMutator: Send + Sync {
    /// Apply `request` to the authoritative document and return the sync to
    /// broadcast. Errors leave the document unchanged (the underlying store ops
    /// are transactional and revision-guarded) and are surfaced verbatim to the
    /// requesting client as a `CommandRejected`.
    fn apply_mutation(&self, request: DocumentMutationRequest) -> DocumentMutationFuture<'_>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(document_id: DocumentId, revision: u64) -> DocumentSync {
        DocumentSync {
            document_id,
            revision,
            update: vec![revision as u8],
        }
    }

    #[tokio::test]
    async fn subscriber_receives_published_syncs_in_order() {
        let hub = DocumentHub::new();
        let doc = DocumentId::new();
        let mut rx = hub.subscribe(doc);

        hub.publish(doc, sync(doc, 1));
        hub.publish(doc, sync(doc, 2));

        assert_eq!(rx.recv().await.unwrap().revision, 1);
        assert_eq!(rx.recv().await.unwrap().revision, 2);
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_a_silent_noop() {
        let hub = DocumentHub::new();
        hub.publish(DocumentId::new(), sync(DocumentId::new(), 1));
        assert_eq!(hub.document_count(), 0);
    }

    #[tokio::test]
    async fn channels_are_isolated_per_document() {
        let hub = DocumentHub::new();
        let a = DocumentId::new();
        let b = DocumentId::new();
        let mut rx_a = hub.subscribe(a);
        let _rx_b = hub.subscribe(b);

        hub.publish(a, sync(a, 7));
        assert_eq!(rx_a.recv().await.unwrap().revision, 7);
        assert_eq!(hub.document_count(), 2);
    }

    #[test]
    fn prune_drops_channels_whose_receivers_detached() {
        let hub = DocumentHub::new();
        let doc = DocumentId::new();
        {
            let _rx = hub.subscribe(doc);
            assert_eq!(hub.document_count(), 1);
        }
        // Receiver dropped at scope end; prune reclaims the channel.
        hub.prune_idle();
        assert_eq!(hub.document_count(), 0);
    }
}
