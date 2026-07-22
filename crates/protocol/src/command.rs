//! Commands: client requests for state changes.
//!
//! A [`Command`] carries an `idempotency_key` so a duplicate delivery produces
//! exactly one effect (the daemon records the first application and replays its
//! recorded result on a repeat — STEP 1.3). Commands request change; the daemon
//! decides, persists, and only then emits the resulting events.

use serde::{Deserialize, Serialize};

use crate::document::{DocumentEditLease, DocumentMutation, PublishTarget};
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
    /// Publish a document's current revision to a Git target (Phase 4 STEP 4.4,
    /// closing the deferred "executing a `PublishPlan`" roadmap item).
    ///
    /// Handled at the connection level like `MutateDocument`/`StartWorkflow`
    /// (documents live outside the session ledger): the daemon computes the
    /// deterministic publish plan, then durably parks its approval — carrying
    /// the plan's target, changed files, and resulting Git action, shown
    /// verbatim on the approval card before any write — through the
    /// assembly's `DocumentPublisher` seam, and replies
    /// [`DocumentPublishRequested`](crate::envelope::Payload::DocumentPublishRequested)
    /// with the parked plan. Nothing is written until a human resolves the
    /// approval through the ordinary `ResolveApproval` command; a rejection
    /// executes nothing. Requires the `Controller` role; a daemon assembled
    /// without a publisher rejects it `document.transport-unavailable`.
    PublishDocument {
        document_id: DocumentId,
        target: PublishTarget,
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
        /// The canonical filesystem root of the repository this workflow's agent
        /// nodes operate on. A per-user daemon can serve several checkouts over
        /// one socket, so the run — not the daemon's startup working directory —
        /// must decide which repository its agent nodes' isolated worktrees are
        /// carved from (Phase 5 T5, fixing P5-D1). Mirrors
        /// [`StartRun.repository`](CommandBody::StartRun): `#[serde(default)]`
        /// keeps an older client (which sends none) working — the daemon then
        /// falls back to its own startup repository root, never a wandering
        /// `current_dir()` at node-execution time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
    },
    /// Pause a running durable workflow run (Phase 5 STEP 5.2 lifecycle command).
    ///
    /// Like `StartWorkflow`, handled at the connection level (a workflow run lives
    /// in its own durable store outside the session ledger): the daemon flips the
    /// run to `paused` through its `WorkflowLifecycle` seam so a live driver stops
    /// launching further nodes (cooperative pause — the in-flight wave finishes),
    /// and the run waits for a `ResumeWorkflow`. Controlling a run is a
    /// [`Controller`](crate::handshake::ClientRole::Controller) capability, so a
    /// lesser role is denied; a terminal run is rejected `workflow.illegal-transition`;
    /// a daemon without workflow transport rejects it `workflow.transport-unavailable`.
    PauseWorkflow {
        workflow_run_id: String,
    },
    /// Resume a paused durable workflow run (Phase 5 STEP 5.2). The daemon validates
    /// the run is paused and drives it onward from its ready frontier in the
    /// background, replying as soon as the resume is accepted. Only a paused run may
    /// be resumed (else `workflow.illegal-transition`); role/transport gating matches
    /// `PauseWorkflow`.
    ResumeWorkflow {
        workflow_run_id: String,
    },
    /// Re-drive a durable workflow run from a chosen node (Phase 5 STEP 5.2
    /// retry-from-node). The daemon resets that node and everything transitively
    /// downstream of it to `pending`, sets the run `running`, and drives in the
    /// background. An unknown `node_id` (or a graph that changed under the run) is
    /// rejected; role/transport gating matches `PauseWorkflow`.
    RetryWorkflowNode {
        workflow_run_id: String,
        /// The node id to re-drive from (its transitive dependents reset with it).
        node_id: String,
    },
    /// Cancel a durable workflow run (Phase 5 STEP 5.2 / T9 — the missing control:
    /// pause/resume/retry exist, cancel did not). Like `PauseWorkflow`, handled at
    /// the connection level and gated to the
    /// [`Controller`](crate::handshake::ClientRole::Controller) role. A cooperative
    /// drain (mirroring pause): the driver stops launching further nodes, any
    /// in-flight node's agent run is interrupted through the same cancellation
    /// machinery `CancelRun` uses, every still-`Pending` node becomes `Skipped`, and
    /// the run lands `Cancelled` — a **terminal** state (no resume from `Cancelled`;
    /// a later resume/pause is rejected `workflow.illegal-transition`). Idempotent on
    /// an already-cancelled run; a daemon without workflow transport rejects it
    /// `workflow.transport-unavailable`.
    CancelWorkflow {
        workflow_run_id: String,
    },
    /// Read a durable workflow run's observability snapshot (Phase 5 STEP 5.2 / T9):
    /// the run's current phase plus every node's full current view (state, attempt,
    /// measured cost, failure/block reason, budget warnings), in topological order.
    /// Like `ReadBlackboard`, intercepted at the connection level (a workflow run
    /// lives in its own durable store outside the session ledger) and served through
    /// the assembly's `WorkflowReader` seam; the daemon replies
    /// [`WorkflowRunSnapshot`](crate::envelope::Payload::WorkflowRunSnapshot). This
    /// is the catch-up baseline a client folds a `Subscription::Workflow` live stream
    /// on top of; reconstructed from the store, so a late subscriber after a restart
    /// still gets a truthful baseline. A **read** — any attached client (an Observer
    /// included) may issue it. An unknown run is rejected `workflow.run-not-found`; a
    /// daemon without workflow transport rejects it `workflow.transport-unavailable`.
    ReadWorkflowRun {
        workflow_run_id: String,
    },
    /// Draft a candidate for the evaluation-gated promotion pipeline (Phase 7
    /// STEP 7.5 — nothing promotes itself, ADR-010).
    ///
    /// Handled at the connection level like `StartWorkflow` (a promotion
    /// candidate lives in its own durable store outside the session ledger):
    /// the daemon creates a draft through its `PromotionGateway` seam and
    /// replies [`Payload::PromotionProposed`](crate::envelope::Payload::PromotionProposed)
    /// with the new candidate id — or `CommandRejected` when a synthesized
    /// candidate needs permission review, or the daemon has no promotion
    /// transport (`promotion.transport-unavailable`). `kind` is the wire name
    /// of an `ArtifactKind` (e.g. `"skill"`, `"router"`); an unrecognized kind
    /// is rejected rather than guessed at.
    ProposePromotion {
        kind: String,
        name: String,
        version: u32,
        #[serde(default)]
        requires_permission_review: bool,
    },
    /// Advance a candidate through the offline-regression / shadow / canary
    /// legs of the pipeline (Phase 7 STEP 7.5). `action` names exactly which
    /// transition to attempt; an illegal transition (wrong stage, or an
    /// unobserved canary trying to finish) is rejected verbatim as the
    /// underlying state-machine error, never silently coerced into success.
    /// Same connection-level handling and role gating as `ProposePromotion`.
    AdvancePromotion {
        candidate_id: String,
        action: PromotionAction,
    },
    /// **Approve and promote a candidate.** The human-approval gate
    /// (ADR-010, exit criterion 2): the daemon authenticates the acting party
    /// as `Actor::Human` from the connection's role — over this local-first
    /// socket, a `Controller`-role connection **is** the human operator (the
    /// same mapping `ResolveApproval` already uses for `resolved_by`) — and
    /// only a `Controller` may issue this command; every other role, and
    /// necessarily every non-human actor, is refused structurally before the
    /// promotion seam is ever invoked. No field on the wire lets a caller
    /// *supply* an actor — that would defeat the whole point of ADR-010.
    ApprovePromotion {
        candidate_id: String,
    },
    /// Manually roll back a promoted candidate to its predecessor version
    /// (Phase 7 STEP 7.5, exit criterion 4: reversible). Requires the
    /// `Controller` role like `ApprovePromotion`, and — unlike approval — the
    /// engine itself does not restrict rollback to a human actor (stopping a
    /// bad change needs no human, only promoting a good one does); the
    /// daemon still attributes the connection's mapped `Actor::Human` so a
    /// manual rollback is never confused with the system-attributed
    /// auto-rollback a canary regression produces on its own.
    RollbackPromotion {
        candidate_id: String,
    },
    /// Read a durable workflow run's blackboard (Phase 5 STEP 5.3): the typed
    /// artifacts its agents posted, optionally filtered by `kind`. Like
    /// `StartWorkflow`, intercepted at the connection level (a workflow run's board
    /// lives in its own durable store outside the session ledger): the daemon reads
    /// it through its `BlackboardReader` seam and replies
    /// [`BlackboardItems`](crate::envelope::Payload::BlackboardItems) with the
    /// matching [`BlackboardItemView`](crate::blackboard::BlackboardItemView)s. This
    /// is a **read** — any attached client, an Observer included, may issue it
    /// (there is no client-facing *post* command; only the workflow executor writes
    /// the board). A daemon assembled without a reader rejects it
    /// `workflow.transport-unavailable`.
    ReadBlackboard {
        workflow_run_id: String,
        /// A blackboard artifact kind to filter by (`finding`, `decision`, …), or
        /// all kinds when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Include superseded revisions too; the default (`false`) returns only the
        /// live board (the "live-only" view the TUI shows).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        include_superseded: bool,
    },
    #[serde(other)]
    Unknown,
}

/// One legal state-machine transition to attempt via `AdvancePromotion`
/// (Phase 7 STEP 7.5). Mirrors `codypendent_eval::promote::Candidate`'s
/// methods exactly; `regressed`/canary-observation verdicts are supplied by
/// the caller — this command *records* a result, it does not compute one (see
/// the crate-level docs on why live shadow/canary traffic capture is a
/// separate, later concern).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum PromotionAction {
    /// Run the offline regression suite; `regressed` is the caller's verdict.
    RunRegression { regressed: bool },
    /// Begin the shadow run.
    StartShadow,
    /// Begin the limited canary.
    StartCanary,
    /// Record one canary signal observation; `regressed` is the caller's
    /// verdict. A regression auto-rolls-back immediately (no human needed to
    /// *stop* a bad change).
    ObserveCanary { regressed: bool },
    /// Finish the canary and assemble the comparison. Refused if no
    /// observation was ever recorded (a canary must not "pass" unobserved).
    FinishCanary,
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
        round_trip(CommandBody::PublishDocument {
            document_id: DocumentId::new(),
            target: crate::document::PublishTarget::RepositoryFile {
                path: "docs/architecture.md".to_string(),
            },
        });
        round_trip(CommandBody::StartWorkflow {
            manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
            inputs: serde_json::json!({ "pull_request": 42 }),
            repository: Some("/home/user/project".to_string()),
        });
        round_trip(CommandBody::PauseWorkflow {
            workflow_run_id: "wfrun-abc123".to_string(),
        });
        round_trip(CommandBody::ResumeWorkflow {
            workflow_run_id: "wfrun-abc123".to_string(),
        });
        round_trip(CommandBody::RetryWorkflowNode {
            workflow_run_id: "wfrun-abc123".to_string(),
            node_id: "verify".to_string(),
        });
        round_trip(CommandBody::CancelWorkflow {
            workflow_run_id: "wfrun-abc123".to_string(),
        });
        round_trip(CommandBody::ReadWorkflowRun {
            workflow_run_id: "wfrun-abc123".to_string(),
        });
        round_trip(CommandBody::ProposePromotion {
            kind: "router".to_string(),
            name: "tool-selection".to_string(),
            version: 12,
            requires_permission_review: false,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::RunRegression { regressed: false },
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::StartShadow,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::StartCanary,
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::ObserveCanary { regressed: true },
        });
        round_trip(CommandBody::AdvancePromotion {
            candidate_id: "cand-abc123".to_string(),
            action: PromotionAction::FinishCanary,
        });
        round_trip(CommandBody::ApprovePromotion {
            candidate_id: "cand-abc123".to_string(),
        });
        round_trip(CommandBody::RollbackPromotion {
            candidate_id: "cand-abc123".to_string(),
        });
        round_trip(CommandBody::ReadBlackboard {
            workflow_run_id: "wfrun-abc123".to_string(),
            kind: Some("finding".to_string()),
            include_superseded: true,
        });
    }

    #[test]
    fn propose_promotion_without_the_review_flag_reparses_to_false() {
        // A payload missing `requires_permission_review` entirely (what an
        // older client, or one hand-constructing the minimal shape, sends)
        // must still parse — defaulted to `false` — rather than erroring.
        let json = serde_json::json!({
            "type": "ProposePromotion",
            "kind": "skill",
            "name": "rust-ci",
            "version": 1,
        });
        let parsed: CommandBody = serde_json::from_value(json).expect("deserialize");
        assert_eq!(
            parsed,
            CommandBody::ProposePromotion {
                kind: "skill".to_string(),
                name: "rust-ci".to_string(),
                version: 1,
                requires_permission_review: false,
            }
        );
    }

    #[test]
    fn unknown_promotion_action_tag_deserializes_to_unknown() {
        // Forward-compatibility (RULE 1) for the nested PromotionAction enum,
        // exactly like CommandBody's own Unknown fallback.
        let parsed: PromotionAction = serde_json::from_value(
            serde_json::json!({ "type": "RunOnnxInference", "confidence": 0.9 }),
        )
        .expect("unknown tag must parse, not error");
        assert!(matches!(parsed, PromotionAction::Unknown));
    }

    #[test]
    fn read_blackboard_omits_default_filter_and_flag() {
        // A live-only, all-kinds read sends neither optional key, and such a payload
        // (also what an older client emits) reparses with both defaulted.
        let body = CommandBody::ReadBlackboard {
            workflow_run_id: "wfrun-abc123".to_string(),
            kind: None,
            include_superseded: false,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("kind"), "absent kind is skipped: {json}");
        assert!(
            !json.contains("include_superseded"),
            "default (false) include_superseded is skipped: {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body);
    }

    #[test]
    fn start_workflow_omits_null_inputs_and_reparses() {
        // A workflow with no inputs sends no `inputs` key, and such a payload
        // (also what an older client emits) reparses with `inputs` defaulted to
        // null.
        let body = CommandBody::StartWorkflow {
            manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
            inputs: serde_json::Value::Null,
            repository: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("inputs"), "null inputs are skipped: {json}");
        assert!(
            !json.contains("repository"),
            "an absent repository is skipped on the wire: {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body, "a payload without either key defaults them");
    }

    #[test]
    fn start_workflow_carries_a_repository_when_present() {
        // A workflow run bound to a repository (Phase 5 T5) serializes the key,
        // and round-trips back to the same value — the durable store persists it
        // so recovery drives the run's agent nodes in the right checkout.
        let body = CommandBody::StartWorkflow {
            manifest: "schema_version: 1\nid: wf\nversion: 1\nsteps: []\n".to_string(),
            inputs: serde_json::Value::Null,
            repository: Some("/home/user/project".to_string()),
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(
            json.contains("/home/user/project"),
            "repository on the wire: {json}"
        );
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
