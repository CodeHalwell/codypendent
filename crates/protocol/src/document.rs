//! Collaborative-document wire types (Phase 4, STEP 4.3).
//!
//! The protocol carries *semantic* document mutations (insert/delete/edit a
//! block, annotate a range as a suggestion, accept/reject a suggestion) plus an
//! opaque CRDT **sync** message for a `Document` subscription. These are wire
//! contracts only: the daemon applies them to the authoritative Loro document in
//! `codypendent-knowledge` (which depends on this crate, not the other way
//! round), so a block's content rides as opaque JSON rather than a typed block —
//! the protocol never needs to know the block schema.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::DocumentId;

/// A semantic mutation on a collaborative document. Internally tagged with an
/// [`DocumentMutation::Unknown`] fallback so a newer client's mutation
/// deserializes and is rejected structurally rather than crashing the peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DocumentMutation {
    /// Insert a block at `index` with the given stable `block_id` and opaque
    /// content (a serialized `BlockContent`).
    Insert {
        index: u32,
        block_id: String,
        content: serde_json::Value,
    },
    /// Delete the block with `block_id`.
    Delete { block_id: String },
    /// A CRDT text edit inside a block: at character `position`, delete
    /// `delete_len` characters then insert `insert`.
    EditText {
        block_id: String,
        position: u32,
        #[serde(default)]
        delete_len: u32,
        #[serde(default)]
        insert: String,
    },
    /// Propose a replacement over a character range of a block (a suggestion).
    Annotate { suggestion: SuggestionInput },
    /// Accept a previously proposed suggestion, applying its range.
    AcceptSuggestion { suggestion_id: String },
    /// Reject a suggestion without applying it.
    RejectSuggestion { suggestion_id: String },
    #[serde(other)]
    Unknown,
}

/// A proposed replacement over `[range_start, range_end)` of a block's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionInput {
    pub block_id: String,
    pub range_start: u32,
    pub range_end: u32,
    pub replacement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

/// A CRDT sync message for a `Document` subscription: opaque Loro update or
/// snapshot bytes plus the document it belongs to. Receivers merge `update` into
/// their local replica; senders emit it after applying a mutation. Opaque so the
/// protocol stays agnostic of the CRDT encoding (ADR-016 chose Loro, but the
/// wire contract does not name it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSync {
    pub document_id: DocumentId,
    /// The document revision this update advances to (for ordering/UX).
    pub revision: u64,
    /// Opaque CRDT bytes. Serialized on the wire as a JSON array of byte values
    /// (see the [`byte_vec`] module) — the framing layer emits plain
    /// `serde_json`, so there is no base64 step; a client sends the raw bytes and
    /// they round-trip as numbers. Large documents are exchanged as incremental
    /// updates rather than full snapshots to stay under the frame-size bound.
    #[serde(with = "byte_vec")]
    pub update: Vec<u8>,
}

/// A request to lease a block range for exclusive writing (Chapter 03 / STEP
/// 4.3). One writer per block-range; readers are unlimited. Reuses the Phase-1
/// lease machinery in the daemon; this is only the wire request shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentEditLease {
    pub document_id: DocumentId,
    /// The block the writer intends to edit; `None` leases the whole document
    /// structure (block insert/delete/reorder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
}

/// A granted document lease — the daemon's reply to an accepted
/// `AcquireDocumentLease` (STEP 4.3 client transport). The client holds the
/// `lease_id` as the capability to renew (a re-acquire of the same range) and to
/// `ReleaseDocumentLease` when it stops editing; `expires_at` is when the lease
/// lapses if neither happens, so a crashed holder never blocks the range forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentLeaseGrant {
    /// The server-minted lease id, returned only to the acquirer.
    pub lease_id: String,
    pub document_id: DocumentId,
    /// The leased block, or `None` for a whole-document (structural) lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    /// When the lease lapses unless renewed.
    pub expires_at: DateTime<Utc>,
}

/// Serialize `Vec<u8>` as a JSON array of numbers (portable, no extra deps). The
/// framing layer already bounds frame size, so document snapshots that would be
/// large are exchanged as incremental updates, not full snapshots.
mod byte_vec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(bytes.iter().copied())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        Vec::<u8>::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(mutation: DocumentMutation) {
        let json = serde_json::to_string(&mutation).expect("serialize");
        let parsed: DocumentMutation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(mutation, parsed);
    }

    #[test]
    fn every_document_mutation_round_trips() {
        round_trip(DocumentMutation::Insert {
            index: 0,
            block_id: "b1".into(),
            content: serde_json::json!({"type": "paragraph", "text": "hi"}),
        });
        round_trip(DocumentMutation::Delete {
            block_id: "b1".into(),
        });
        round_trip(DocumentMutation::EditText {
            block_id: "b1".into(),
            position: 2,
            delete_len: 1,
            insert: "x".into(),
        });
        round_trip(DocumentMutation::Annotate {
            suggestion: SuggestionInput {
                block_id: "b1".into(),
                range_start: 0,
                range_end: 3,
                replacement: "new".into(),
                rationale: Some("clearer".into()),
            },
        });
        round_trip(DocumentMutation::AcceptSuggestion {
            suggestion_id: "s1".into(),
        });
        round_trip(DocumentMutation::RejectSuggestion {
            suggestion_id: "s1".into(),
        });
    }

    #[test]
    fn unknown_mutation_op_deserializes_to_unknown() {
        let parsed: DocumentMutation =
            serde_json::from_value(serde_json::json!({ "op": "teleport_block" }))
                .expect("unknown op must parse, not error");
        assert!(matches!(parsed, DocumentMutation::Unknown));
    }

    #[test]
    fn document_sync_round_trips() {
        let sync = DocumentSync {
            document_id: DocumentId::new(),
            revision: 7,
            update: vec![1, 2, 3, 255],
        };
        let json = serde_json::to_string(&sync).expect("serialize");
        let parsed: DocumentSync = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sync, parsed);
    }

    #[test]
    fn document_lease_grant_round_trips() {
        let grant = DocumentLeaseGrant {
            lease_id: "lease-1".into(),
            document_id: DocumentId::new(),
            block_id: Some("b1".into()),
            expires_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let json = serde_json::to_string(&grant).expect("serialize");
        let parsed: DocumentLeaseGrant = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(grant, parsed);
    }

    #[test]
    fn a_whole_document_grant_omits_the_block() {
        // A structural lease carries no block; the field is skipped on the wire and
        // reparses to `None`.
        let grant = DocumentLeaseGrant {
            lease_id: "lease-2".into(),
            document_id: DocumentId::new(),
            block_id: None,
            expires_at: Utc::now(),
        };
        let json = serde_json::to_string(&grant).expect("serialize");
        assert!(!json.contains("block_id"));
        let parsed: DocumentLeaseGrant = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.block_id, None);
    }
}
