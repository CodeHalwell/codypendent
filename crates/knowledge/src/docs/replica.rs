//! The client-side CRDT replica (Phase 4 STEP 4.3 client wiring).
//!
//! A [`DocumentReplica`] is the *client's* mirror of the daemon's authoritative
//! Loro document. It is the last engine-side piece of Phase 4's collaboration
//! slice: the daemon already applies a `MutateDocument` onto the authoritative
//! CRDT and fans the resulting [`DocumentSync`] out to `Document` subscribers;
//! this is what a subscriber *consumes* that stream with.
//!
//! It reuses the exact same [`DocumentCrdt`] layer the daemon writes through
//! (the block↔CRDT bijection of [`super::crdt`]), so a snapshot it seeds from and
//! the [`DocumentSync`] updates it merges share **one Loro history**. That
//! matters: rebuilding a replica from the block projection ([`DocumentCrdt::from_blocks`])
//! would mint fresh operation ids, so merging the daemon's snapshot afterwards
//! would union two independent histories and *duplicate every block*. Seeding
//! from the daemon's own snapshot bytes keeps the histories identical, so every
//! merge is a true idempotent CRDT union.
//!
//! Lifecycle on the client:
//! 1. seed once from the current document state — the **document read path**, a
//!    daemon-persisted CRDT snapshot — via [`DocumentReplica::from_snapshot`], or
//!    start [`DocumentReplica::empty`] and converge from the first live sync
//!    (every `DocumentSync.update` is a *full* idempotent snapshot, so an empty
//!    replica converges on its first merge — no separate initial-state frame is
//!    needed);
//! 2. [`DocumentReplica::merge`] every incoming `DocumentSync` as the
//!    authoritative replica advances — idempotent, so a duplicated, delayed, or
//!    dropped-then-resent sync self-heals;
//! 3. project the block-structured view the editor renders via
//!    [`DocumentReplica::blocks`], or the deterministic Markdown the reviewer sees
//!    via [`DocumentReplica::render`].
//!
//! The replica holds no database or socket handle: the client harness owns the
//! socket (it hands each `DocumentSync` here) and the read path (it hands the
//! seed snapshot here). This keeps the replica a pure, unit-testable CRDT mirror.

use codypendent_protocol::document::DocumentSync;

use super::crdt::{DocCrdtError, DocumentCrdt};
use super::model::DocumentBlock;
use super::render::render_document;

/// A client's live replica of one collaborative document, backed by the shared
/// [`DocumentCrdt`] layer and advanced by [`DocumentSync`] merges.
pub struct DocumentReplica {
    crdt: DocumentCrdt,
    /// The highest document revision any merged (or seeded) sync reported — a UX
    /// hint (which revision the rendered content reflects), never the merge
    /// authority. CRDT convergence, not this counter, decides content.
    revision: u64,
}

impl Default for DocumentReplica {
    fn default() -> Self {
        Self::empty()
    }
}

impl DocumentReplica {
    /// An empty replica. Converges to the authoritative content on the first
    /// [`DocumentReplica::merge`], because every `DocumentSync.update` is a full
    /// idempotent snapshot.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            crdt: DocumentCrdt::new(),
            revision: 0,
        }
    }

    /// Seed from the current document state — a daemon-persisted CRDT snapshot
    /// obtained over the document read path (e.g. `Document::crdt.snapshot()` read
    /// from the store) — anchored to the revision that snapshot is at. Subsequent
    /// [`DocumentReplica::merge`]s advance it.
    pub fn from_snapshot(bytes: &[u8], revision: u64) -> Result<Self, DocCrdtError> {
        Ok(Self {
            crdt: DocumentCrdt::from_snapshot(bytes)?,
            revision,
        })
    }

    /// Merge one incoming [`DocumentSync`] from the daemon's fan-out. Idempotent:
    /// re-merging the same sync (a duplicate delivery, or a snapshot that overlaps
    /// one already applied) leaves the content unchanged, so a lossy live stream
    /// self-heals on the next sync it does see.
    pub fn merge(&mut self, sync: &DocumentSync) -> Result<(), DocCrdtError> {
        self.crdt.merge_snapshot(&sync.update)?;
        // The revision is a monotone UX hint: a delayed older sync must not walk
        // the displayed revision backwards even though its (idempotent) content
        // merge is harmless.
        self.revision = self.revision.max(sync.revision);
        Ok(())
    }

    /// The current block-structured projection the editor renders.
    pub fn blocks(&self) -> Result<Vec<DocumentBlock>, DocCrdtError> {
        self.crdt.to_blocks()
    }

    /// The revision the rendered content currently reflects (the highest seen).
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The deterministic Markdown render of the current content, byte-identical to
    /// what the daemon renders for the same block list (STEP 4.4) — the form two
    /// converged replicas compare equal on.
    pub fn render(&self, title: &str) -> Result<String, DocCrdtError> {
        Ok(render_document(title, &self.blocks()?))
    }

    /// Export the replica's current CRDT snapshot (its whole history). Used to
    /// hand a freshly opened peer a seed, or to assert convergence in tests.
    pub fn snapshot(&self) -> Result<Vec<u8>, DocCrdtError> {
        self.crdt.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::model::BlockContent;
    use codypendent_protocol::DocumentId;

    /// Build a `DocumentSync` the way the daemon does: a full snapshot export of
    /// the authoritative CRDT at some revision.
    fn sync_from(crdt: &DocumentCrdt, document_id: DocumentId, revision: u64) -> DocumentSync {
        DocumentSync {
            document_id,
            revision,
            update: crdt.snapshot().expect("snapshot"),
        }
    }

    fn paragraph(id: &str, text: &str) -> DocumentBlock {
        DocumentBlock::with_id(
            id,
            BlockContent::Paragraph {
                text: text.to_owned(),
            },
        )
    }

    #[test]
    fn empty_replica_converges_from_the_first_sync() {
        // The authoritative document, edited on the daemon side.
        let authoritative =
            DocumentCrdt::from_blocks(&[paragraph("p", "hello world")]).expect("build");
        let doc = DocumentId::new();

        // A subscriber that started empty converges the moment it sees one full
        // snapshot — no separate initial-state frame required.
        let mut replica = DocumentReplica::empty();
        replica
            .merge(&sync_from(&authoritative, doc, 2))
            .expect("merge");

        assert_eq!(
            replica.blocks().expect("blocks"),
            authoritative.to_blocks().expect("blocks"),
        );
        assert_eq!(replica.revision(), 2);
    }

    #[test]
    fn merging_the_same_update_twice_yields_identical_state() {
        // The core idempotency invariant: apply the same update twice ⇒ same state
        // (the daemon's fan-out may redeliver, and the seed can overlap the first
        // live sync).
        let authoritative =
            DocumentCrdt::from_blocks(&[paragraph("a", "one"), paragraph("b", "two")])
                .expect("build");
        let doc = DocumentId::new();
        let update = sync_from(&authoritative, doc, 5);

        let mut once = DocumentReplica::empty();
        once.merge(&update).expect("merge once");
        let after_once = once.blocks().expect("blocks");

        let mut twice = DocumentReplica::empty();
        twice.merge(&update).expect("merge 1");
        twice.merge(&update).expect("merge 2 (idempotent)");
        let after_twice = twice.blocks().expect("blocks");

        assert_eq!(after_once, after_twice);
        assert_eq!(after_twice, authoritative.to_blocks().expect("blocks"));
        // A redelivered older sync must not walk the revision backwards.
        twice
            .merge(&sync_from(&authoritative, doc, 3))
            .expect("old");
        assert_eq!(twice.revision(), 5);
    }

    #[test]
    fn seeding_from_a_snapshot_projects_the_blocks() {
        let authoritative = DocumentCrdt::from_blocks(&[paragraph("p", "seeded")]).expect("build");
        let snapshot = authoritative.snapshot().expect("snapshot");

        let replica = DocumentReplica::from_snapshot(&snapshot, 4).expect("seed");
        assert_eq!(
            replica.blocks().expect("blocks"),
            authoritative.to_blocks().expect("blocks"),
        );
        assert_eq!(replica.revision(), 4);
    }

    #[test]
    fn a_seeded_replica_then_merges_a_later_edit() {
        // Seed from the read path, then converge onto a later authoritative edit
        // delivered live — the two-step lifecycle the client uses.
        let authoritative = DocumentCrdt::from_blocks(&[paragraph("p", "draft")]).expect("build");
        let doc = DocumentId::new();
        let seed = authoritative.snapshot().expect("snapshot");
        let mut replica = DocumentReplica::from_snapshot(&seed, 1).expect("seed");

        // The daemon edits the SAME crdt (shared history) and republishes.
        authoritative
            .insert_text("p", 5, " final")
            .expect("edit text");
        replica
            .merge(&sync_from(&authoritative, doc, 2))
            .expect("merge edit");

        assert_eq!(
            replica.render("Doc").expect("render"),
            render_document("Doc", &authoritative.to_blocks().expect("blocks")),
        );
        assert!(replica
            .render("Doc")
            .expect("render")
            .contains("draft final"));
    }

    #[test]
    fn two_replicas_of_the_same_stream_render_identically() {
        // "B converges to identical rendered content": two independent replicas
        // fed the same sync stream render byte-identical Markdown.
        let authoritative = DocumentCrdt::from_blocks(&[paragraph("p", "shared")]).expect("build");
        let doc = DocumentId::new();

        let mut a = DocumentReplica::empty();
        let mut b = DocumentReplica::empty();
        let s1 = sync_from(&authoritative, doc, 1);
        a.merge(&s1).expect("a1");
        b.merge(&s1).expect("b1");

        authoritative.insert_text("p", 6, "!").expect("edit");
        let s2 = sync_from(&authoritative, doc, 2);
        // Deliver in a different order / with a duplicate to B — convergence holds.
        a.merge(&s2).expect("a2");
        b.merge(&s2).expect("b2");
        b.merge(&s2).expect("b2 duplicate");

        assert_eq!(a.render("T").expect("a"), b.render("T").expect("b"));
        assert!(a.render("T").expect("a").contains("shared!"));
    }
}
