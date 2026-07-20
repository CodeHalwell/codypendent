//! Commands: client requests for state changes.
//!
//! A [`Command`] carries an `idempotency_key` so a duplicate delivery produces
//! exactly one effect (the daemon records the first application and replays its
//! recorded result on a repeat — STEP 1.3). Commands request change; the daemon
//! decides, persists, and only then emits the resulting events.

use serde::{Deserialize, Serialize};

use crate::document::{DocumentEditLease, DocumentMutation};
use crate::handshake::{ClientRole, Subscription};
use crate::ide::IdeContextUpdate;
use crate::ids::{ApprovalId, CommandId, DocumentId, RunId, SessionId, WorkspaceId};
use crate::run::{AgentMode, ApprovalDecision, ApprovalScope};

/// An idempotent, optionally revision-guarded request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub command_id: CommandId,
    /// Client-chosen key; the same key must never apply twice.
    pub idempotency_key: String,
    /// Optimistic-concurrency guard: apply only if the session is still at this
    /// revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub body: CommandBody,
}

/// The specific change a command requests. A wire enum: internally tagged with
/// an [`CommandBody::Unknown`] fallback so a command from a newer client
/// deserializes and is rejected structurally rather than crashing the peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum CommandBody {
    CreateSession {
        workspace: WorkspaceId,
        title: String,
    },
    AttachSession {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_seen_sequence: Option<u64>,
        subscriptions: Vec<Subscription>,
        requested_role: ClientRole,
    },
    SubmitUserInput {
        session_id: SessionId,
        text: String,
        mode: AgentMode,
    },
    StartRun {
        session_id: SessionId,
        objective: String,
        mode: AgentMode,
        /// The canonical filesystem root of the repository this run operates on.
        /// A per-user daemon can serve several checkouts over one socket, so the
        /// run — not the daemon's startup working directory — must decide which
        /// repository its context map and curated memories are attributed to
        /// (issue #6 item 1). `#[serde(default)]` keeps an older client (which
        /// sends none) working: the daemon then falls back to its own directory,
        /// exactly as before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
    },
    ResolveApproval {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    },
    CancelRun {
        run_id: RunId,
    },
    PauseRun {
        run_id: RunId,
    },
    ResumeRun {
        run_id: RunId,
    },
    QueueSteering {
        run_id: RunId,
        text: String,
    },
    /// Push the IDE's live context (active file, selection, open documents, and
    /// unsaved-buffer digests) for a session (Phase 3 STEP 3.4). Latest-wins and
    /// high-frequency (debounced ≥ 300 ms client-side), so the daemon stores it
    /// as a projection outside the event ledger rather than appending an event.
    UpdateIdeContext {
        session_id: SessionId,
        update: IdeContextUpdate,
    },
    /// Apply a semantic mutation to a collaborative document (Phase 4 STEP 4.3).
    ///
    /// Handled at the connection level (documents live outside the session
    /// ledger, so this never flows through the event write path): the daemon
    /// applies it onto the authoritative Loro document through its
    /// `DocumentMutator` seam — mode-gated by the document's scope (content
    /// edits become suggestions outside `Edit` mode), single-writer enforced
    /// via the edit-lease `require` pre-check — and fans the resulting
    /// `DocumentSync` out to the document's subscribers. An Observer is
    /// role-denied; a daemon assembled without a mutator rejects it
    /// `document.transport-unavailable`.
    MutateDocument {
        document_id: DocumentId,
        mutation: DocumentMutation,
    },
    /// Acquire (or renew) an edit lease over a document block-range before editing
    /// it (Phase 4 STEP 4.3 client transport). One writer per block-range: a
    /// whole-document lease (`block_id = None`) covers structural edits and
    /// conflicts with any block lease. The daemon replies
    /// [`DocumentLeaseGranted`](crate::envelope::Payload::DocumentLeaseGranted)
    /// with the minted lease id + expiry, or `CommandRejected` `document.range-leased`
    /// when a different writer holds an overlapping range. Like `MutateDocument`
    /// this is intercepted at the connection level (documents live outside the
    /// session ledger) rather than flowing through the event write path.
    AcquireDocumentLease {
        lease: DocumentEditLease,
        /// How long the lease is valid, in seconds; the daemon applies a default
        /// when absent. A re-acquire by the same holder renews the expiry in place.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_seconds: Option<u64>,
    },
    /// Release a previously acquired document lease by its id (Phase 4 STEP 4.3).
    /// Idempotent — releasing an already-released or unknown lease is accepted as a
    /// no-op — so a client that loses the acknowledgement can retry safely.
    ReleaseDocumentLease {
        lease_id: String,
    },
    /// Start a durable workflow run from a compiled manifest (Phase 5 STEP 5.2).
    ///
    /// Carries the workflow **manifest YAML** (its content, not a path — the daemon
    /// never reads an arbitrary client-named file) and the typed `inputs` the
    /// manifest declares. Handled at the connection level like `MutateDocument` (a
    /// workflow run lives in its own durable store outside the session ledger): the
    /// daemon compiles the manifest, creates the run through its `WorkflowStarter`
    /// seam, and replies
    /// [`WorkflowRunStarted`](crate::envelope::Payload::WorkflowRunStarted) with the
    /// new run id — or `CommandRejected` when the manifest does not compile. A
    /// daemon assembled without a starter rejects it `workflow.transport-unavailable`.
    /// (Driving the created run is a later step; this command only makes runs
    /// durably creatable.)
    StartWorkflow {
        /// The workflow manifest YAML (the content of a `workflow.yaml`).
        manifest: String,
        /// The typed inputs the manifest declares, as JSON. Defaults to null when
        /// omitted (a workflow with no required inputs).
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        inputs: serde_json::Value,
    },
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(body: CommandBody) {
        let command = Command {
            command_id: CommandId::new(),
            idempotency_key: "idem-1".to_string(),
            expected_revision: Some(7),
            body,
        };
        let json = serde_json::to_string(&command).expect("serialize");
        let parsed: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(command, parsed);
    }

    #[test]
    fn start_run_repository_is_omitted_when_absent_and_reparses_to_none() {
        // The per-run repository (issue #6 item 1) is optional on the wire: a
        // client that sends none produces JSON without the key, and such a
        // payload (also what an older client emits) parses back to `None` so the
        // daemon falls back to its own directory.
        let body = CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
            repository: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(
            !json.contains("repository"),
            "an absent repository is skipped on the wire: {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body, "a payload without the key defaults to None");
    }

    #[test]
    fn every_command_body_round_trips() {
        round_trip(CommandBody::CreateSession {
            workspace: WorkspaceId::new(),
            title: "fix the failing test".to_string(),
        });
        round_trip(CommandBody::AttachSession {
            session_id: SessionId::new(),
            last_seen_sequence: Some(42),
            subscriptions: vec![Subscription::SessionSummary],
            requested_role: ClientRole::Contributor,
        });
        round_trip(CommandBody::SubmitUserInput {
            session_id: SessionId::new(),
            text: "try again".to_string(),
            mode: AgentMode::Build,
        });
        round_trip(CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "diagnose the failing test".to_string(),
            mode: AgentMode::Build,
            repository: Some("/home/user/project".to_string()),
        });
        round_trip(CommandBody::ResolveApproval {
            approval_id: ApprovalId::new(),
            decision: ApprovalDecision::Approve,
            scope: ApprovalScope::Run,
        });
        round_trip(CommandBody::CancelRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::PauseRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::ResumeRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::QueueSteering {
            run_id: RunId::new(),
            text: "focus on the parser".to_string(),
        });
        round_trip(CommandBody::UpdateIdeContext {
            session_id: SessionId::new(),
            update: IdeContextUpdate {
                active_file: Some("src/lib.rs".to_string()),
                dirty_buffers: vec![crate::ide::DirtyBufferDigest {
                    path: "src/lib.rs".to_string(),
                    sha256: "deadbeef".to_string(),
                    byte_length: 12,
                }],
                ..Default::default()
            },
        });
        round_trip(CommandBody::MutateDocument {
            document_id: DocumentId::new(),
            mutation: DocumentMutation::EditText {
                block_id: "b1".to_string(),
                position: 0,
                delete_len: 0,
                insert: "hello".to_string(),
            },
        });
        round_trip(CommandBody::AcquireDocumentLease {
            lease: DocumentEditLease {
                document_id: DocumentId::new(),
                block_id: Some("b1".to_string()),
            },
            ttl_seconds: Some(300),
        });
        round_trip(CommandBody::ReleaseDocumentLease {
            lease_id: "lease-1".to_string(),
        });
        round_trip(CommandBody::StartWorkflow {
            manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
            inputs: serde_json::json!({ "pull_request": 42 }),
        });
    }

    #[test]
    fn start_workflow_omits_null_inputs_and_reparses() {
        // A workflow with no inputs sends no `inputs` key, and such a payload
        // (also what an older client emits) reparses with `inputs` defaulted to
        // null.
        let body = CommandBody::StartWorkflow {
            manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
            inputs: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("inputs"), "null inputs are skipped: {json}");
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    #[test]
    fn acquire_document_lease_omits_absent_ttl_and_block() {
        // A whole-document lease with the default TTL sends neither optional key,
        // and such a payload (also what an older client would emit) reparses with
        // both defaulted.
        let body = CommandBody::AcquireDocumentLease {
            lease: DocumentEditLease {
                document_id: DocumentId::new(),
                block_id: None,
            },
            ttl_seconds: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("ttl_seconds"));
        assert!(!json.contains("block_id"));
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    #[test]
    fn attach_session_omits_absent_sequence() {
        let command = Command {
            command_id: CommandId::new(),
            idempotency_key: "idem-2".to_string(),
            expected_revision: None,
            body: CommandBody::AttachSession {
                session_id: SessionId::new(),
                last_seen_sequence: None,
                subscriptions: vec![],
                requested_role: ClientRole::Observer,
            },
        };
        let json = serde_json::to_string(&command).expect("serialize");
        assert!(!json.contains("last_seen_sequence"));
        assert!(!json.contains("expected_revision"));
        let parsed: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(command, parsed);
    }

    #[test]
    fn unknown_command_tag_deserializes_to_unknown() {
        let parsed: CommandBody = serde_json::from_value(
            serde_json::json!({ "type": "TeleportRepository", "coords": [1, 2] }),
        )
        .expect("unknown tag must parse, not error");
        assert!(matches!(parsed, CommandBody::Unknown));
    }
}
