//! Durable session events.
//!
//! Events record accepted state changes or observations. They are persisted
//! in the event ledger before any client observes them, and original events
//! are immutable evidence (invariant 5). The Phase 0 seed (session lifecycle)
//! is joined here by the Phase 1 run, model, tool, approval, patch, steering,
//! and budget events. Bulk content is referenced through [`ArtifactRef`]; a
//! bounded human preview may accompany it when clients need an immediately
//! useful timeline card.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactRef;
use crate::handshake::ClientRole;
use crate::ids::{
    AgentId, ApprovalId, ChangeSetId, CheckpointId, ClientId, CommandId, CorrelationId, LearningId,
    ModelId, QuestionId, RunId, SessionId, UserId,
};
use crate::question::{QuestionOutcome, QuestionPrompt};
use crate::run::{
    AgentMode, ApprovalDecision, BudgetDimension, CheckpointKind, PendingPromptView,
    ProposedAction, Risk, RunDisposition, RunState, ToolOutcome,
};

/// Completeness of the measured subtotal. Requests count calls to the model
/// driver, including failed/cancelled calls; each measured dimension counts
/// the calls that reported it. Retries without their own usage leave the
/// subtotal partial even when the final attempt reports a measurement.
/// Absent metadata on older events means coverage is unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct UsageCoverage {
    pub requests: u64,
    pub prompt_requests: u64,
    pub completion_requests: u64,
    pub cost_requests: u64,
    #[serde(default)]
    pub unmeasured_retries: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct SessionEvent {
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<CommandId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<CorrelationId>,
    pub actor: Actor,
    pub body: EventBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum Actor {
    Human {
        user_id: UserId,
    },
    Agent {
        agent_id: AgentId,
        run_id: RunId,
        model: ModelId,
    },
    Client {
        client_id: ClientId,
    },
    Integration {
        integration_id: String,
    },
    System,
    #[serde(other)]
    Unknown,
}

/// The body of a persisted event.
///
/// Internally tagged with a `#[serde(other)] Unknown` fallback (RULE 1): an
/// event type produced by a newer daemon deserializes to `Unknown` in an older
/// client instead of failing the whole frame, and the client renders an
/// "unsupported item" placeholder. Phase 0 variants are preserved so old ledger
/// bytes parse forever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum EventBody {
    // --- Phase 0: session lifecycle ---
    SessionCreated {
        title: String,
    },
    NoteAppended {
        text: String,
        /// The run this note belongs to, when it is run-scoped (a run's context
        /// manifest or a curated-memory note). `None` for a session-level note
        /// (e.g. user input, an effect-reconciliation record), which a client
        /// attaches to whatever run is in focus. Without this, a run's note could
        /// land on the wrong transcript when runs interleave (issue #6 item 3).
        /// `#[serde(default)]` keeps old ledger bytes (which have no `run_id`)
        /// parsing to `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
    },
    SessionClosed,

    // --- Phase 1: run lifecycle and agent activity ---
    RunStarted {
        run_id: RunId,
        objective: String,
        mode: AgentMode,
    },
    RunStateChanged {
        run_id: RunId,
        state: RunState,
    },
    /// A chunk of model output for `run_id`.
    ///
    /// `thought` marks the chunk as the model's REASONING rather than its
    /// reply. ACP already separates the two — `AgentThoughtChunk` versus
    /// `AgentMessageChunk` — and every ACP agent this daemon drives emits both;
    /// before this field the bridge merged them, so a model that narrates its
    /// own deliberation buried its answer under it.
    ///
    /// Deliberately a FIELD rather than a new variant. A new variant would be
    /// unknown to older clients, which under RULE 1 render nothing for it — so
    /// upgrading the daemon would make reasoning text vanish from an old TUI
    /// instead of merely being unfolded. With a defaulted field an old client
    /// deserializes the chunk as an ordinary delta and shows exactly what it
    /// shows today, while a new one can fold it away.
    ModelStreamDelta {
        run_id: RunId,
        text: String,
        /// `true` when this chunk is reasoning, not reply. Defaults to `false`
        /// so a payload written before this field existed still parses.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        thought: bool,
    },
    /// The daemon's model request failed transiently and the driver is waiting
    /// out a backoff before retry `attempt` of `max_attempts`. Purely
    /// informational: a run that ultimately fails still ends with its own
    /// `RunStateChanged`/`RunCompleted`; a retry that succeeds is followed by
    /// ordinary `ModelStreamDelta`s. Additive: an older client deserializes
    /// this to `Unknown` (RULE 1) and renders a placeholder.
    ModelRetrying {
        run_id: RunId,
        attempt: u32,
        max_attempts: u32,
        /// Bounded classifier reason (e.g. "provider is overloaded").
        message: String,
        /// The wait before the retry fires, in milliseconds.
        delay_ms: u64,
    },
    ToolProposed {
        run_id: RunId,
        approval_id: ApprovalId,
        action: ProposedAction,
    },
    /// A model-proposed action that the policy engine refused before
    /// execution. Keeping the typed action makes policy-denial evaluation and
    /// audit evidence non-vacuous: observers can prove the unsafe operation
    /// was attempted and blocked, not merely absent from an execution list.
    ToolDenied {
        run_id: RunId,
        action: ProposedAction,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasons: Vec<String>,
    },
    ToolStarted {
        run_id: RunId,
        /// Tool name, e.g. `shell.run`.
        tool: String,
        /// Digest of the tool arguments (not the arguments themselves).
        args_digest: String,
        /// A short, human-readable display label for the call — e.g. the file
        /// path a `workspace.read_file` targets, or the command a `shell.run`
        /// executes — so a client can render `workspace.read_file ·
        /// services/main.py` instead of the bare tool name. Derived by the
        /// emitter (`codypendent_runtime::tools::tool_label`) from the same
        /// arguments `args_digest` hashes, BEFORE they are discarded: bounded,
        /// single-line, and never the full arguments or file contents.
        /// `#[serde(default)]` keeps old ledger bytes and an older daemon's
        /// events (neither carries this field) deserializing to `None` —
        /// additive and back-compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Refines the display details of an existing call without starting another
    /// tool execution. ACP agents commonly publish the command or path after
    /// their initial generic tool title. `tool` retains the call's identity.
    /// Older clients fold this additive variant to `Unknown` and retain the
    /// initial label and eventual completion.
    ToolUpdated {
        run_id: RunId,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    ToolCompleted {
        run_id: RunId,
        tool: String,
        outcome: ToolOutcome,
        /// Bulk output, if any, as an artifact reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<ArtifactRef>,
    },
    PatchProposed {
        run_id: RunId,
        changeset_id: ChangeSetId,
        /// The patch/diff, stored as an artifact.
        artifact: ArtifactRef,
        /// Repository-relative paths touched by the change set.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
        /// Added lines in the unified diff.
        #[serde(default, skip_serializing_if = "is_zero")]
        additions: u64,
        /// Removed lines in the unified diff.
        #[serde(default, skip_serializing_if = "is_zero")]
        deletions: u64,
        /// A bounded unified-diff preview for immediate review in clients.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        preview: String,
        /// Whether the full artifact contains more diff than `preview`.
        #[serde(default, skip_serializing_if = "is_false")]
        preview_truncated: bool,
    },
    ApprovalRequested {
        approval_id: ApprovalId,
        action: ProposedAction,
        risk: Risk,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    SteeringQueued {
        run_id: RunId,
    },
    SteeringApplied {
        run_id: RunId,
    },
    BudgetWarning {
        run_id: RunId,
        dimension: BudgetDimension,
        used: u64,
        limit: u64,
    },
    /// Live context-window usage, emitted per turn while the model's window is
    /// known (deduped by percent). Distinct from BudgetWarning (a threshold
    /// warning) and RunUsage (a terminal measured tally).
    ContextUsage {
        run_id: RunId,
        used_tokens: u64,
        window_tokens: u64,
        system_tokens: u64,
        tool_tokens: u64,
        transcript_tokens: u64,
    },
    RunCompleted {
        run_id: RunId,
        disposition: RunDisposition,
        /// The run chronicle, stored as a JSON artifact.
        chronicle: ArtifactRef,
    },
    /// What a run actually consumed, as **measured** by the daemon after the
    /// loop finished (outcome 20). This is the wire half of migration 0032's
    /// `runs.prompt_tokens` / `completion_tokens` / `cost_micros`: without it a
    /// client can only ever learn a run's cost by reading the daemon's database
    /// directly, which no client does.
    ///
    /// [`BudgetWarning`](EventBody::BudgetWarning) is deliberately NOT this
    /// event: it reports a limit being approached, so it says nothing at all
    /// about a run with no configured budget — the common case. Usage is a
    /// fact about the run; a warning is a fact about a policy.
    ///
    /// Every dimension is optional because an unmeasured one must stay absent
    /// rather than be reported as zero: a provider that returns no token counts,
    /// or a model with no price on file, would otherwise make a run read as
    /// free. Clients render only what is present (`cost_micros` is USD
    /// millionths).
    RunUsage {
        run_id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completion_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_micros: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        coverage: Option<UsageCoverage>,
    },
    /// A content-free projection of newly curated learning after a successful
    /// run. Facts, provenance, and confidence stay in the governed learning
    /// store; clients receive only counts and opaque ids for review affordances.
    LearningsCaptured {
        run_id: RunId,
        proposed_count: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        proposed_ids: Vec<LearningId>,
        activated_count: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        activated_ids: Vec<LearningId>,
    },

    /// A client attached to or detached from the session (Phase 3 STEP 3.7).
    /// Emitted so every attached client can show who else is present — e.g. the
    /// TUI showing that VS Code has joined the same session during a handoff.
    ClientPresenceChanged {
        client_id: ClientId,
        role: ClientRole,
        /// `true` when the client attached, `false` when it detached.
        present: bool,
    },

    /// The run asked the operator structured questions and parked (adoption 03).
    QuestionAsked {
        question_id: QuestionId,
        run_id: RunId,
        questions: Vec<QuestionPrompt>,
    },
    /// The parked question was answered, rejected, or expired.
    QuestionResolved {
        question_id: QuestionId,
        outcome: QuestionOutcome,
    },

    /// A filesystem checkpoint of the run's operating worktree was recorded
    /// (Adoption 04). `ordinal` is the 1-based user-turn ordinal within the
    /// run: 1 at launch, +1 per applied steering turn. `commit` is the
    /// checkpoint object's SHA; `kind` says how to restore it (`"stash"` needs
    /// `git stash apply`, `"commit"` is a plain reset target). The ref
    /// `refs/codypendent/checkpoints/<run_id>/<ordinal>` pins the object.
    CheckpointRecorded {
        run_id: RunId,
        checkpoint_id: CheckpointId,
        ordinal: u32,
        kind: CheckpointKind,
        commit: String,
        /// The commit the run's worktree was carved from — the "state before
        /// this turn" restore/fork target for a `commit`-kind checkpoint.
        base_commit: String,
    },
    /// A checkpoint restore finished (Adoption 04). `restored` is false when
    /// the transactional restore failed and was rolled back losslessly.
    CheckpointRestored {
        run_id: RunId,
        checkpoint_id: CheckpointId,
        restored: bool,
    },

    /// This session was created by forking another at a checkpoint
    /// (Adoption 05). Appended once, immediately after the copied history, so
    /// the fork's own ledger records its origin. Clients render it as a
    /// "forked from …" marker; `Unknown` on older builds (RULE 1).
    SessionForked {
        from_session: SessionId,
        checkpoint: CheckpointId,
    },

    /// Full snapshot of the session's server-side pending-prompt queue after
    /// a mutation (Adoption 06). Latest-wins: a client folds it by REPLACING
    /// its queue projection, so replaying history converges on the final
    /// queue. Emitted from the same transaction as the mutation it records.
    PendingPromptsChanged {
        prompts: Vec<PendingPromptView>,
    },

    /// Forward-compatibility fallback for an event type this build does not
    /// know (RULE 1). Receivers render a placeholder and continue.
    #[serde(other)]
    Unknown,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::DataClassification;
    use crate::ids::ArtifactId;
    use crate::question::QuestionOption;
    use crate::run::{ApprovalDecision, BudgetDimension, RiskLevel};

    fn artifact_ref() -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: "text/x-diff".to_string(),
            byte_length: 128,
            sha256: "0".repeat(64),
            sensitivity: DataClassification::Internal,
        }
    }

    fn event_with(body: EventBody) -> SessionEvent {
        SessionEvent {
            sequence: 9,
            occurred_at: Utc::now(),
            causation_id: Some(CommandId::new()),
            correlation_id: Some(CorrelationId::new()),
            actor: Actor::Agent {
                agent_id: AgentId::new(),
                run_id: RunId::new(),
                model: ModelId("gpt-5.1-codex".to_string()),
            },
            body,
        }
    }

    fn round_trip(body: EventBody) {
        let event = event_with(body);
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: SessionEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, parsed);
    }

    #[test]
    fn every_phase1_event_body_round_trips() {
        let run_id = RunId::new();
        round_trip(EventBody::RunStarted {
            run_id,
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
        });
        round_trip(EventBody::RunStateChanged {
            run_id,
            state: RunState::Running,
        });
        round_trip(EventBody::ModelStreamDelta {
            run_id,
            text: "thinking...".to_string(),
            thought: false,
        });
        round_trip(EventBody::ModelStreamDelta {
            run_id,
            text: "the user wants the parser rewritten".to_string(),
            thought: true,
        });
        round_trip(EventBody::ModelRetrying {
            run_id,
            attempt: 2,
            max_attempts: 5,
            message: "provider is overloaded".to_string(),
            delay_ms: 4231,
        });
        round_trip(EventBody::ToolProposed {
            run_id,
            approval_id: ApprovalId::new(),
            action: ProposedAction::ExecuteCommand {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
        });
        round_trip(EventBody::ToolDenied {
            run_id,
            action: ProposedAction::ExecuteCommand {
                program: "rm".to_string(),
                args: vec!["-rf".to_string(), "target".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            reasons: vec!["program is not allow-listed".to_string()],
        });
        round_trip(EventBody::ToolStarted {
            run_id,
            tool: "shell.run".to_string(),
            args_digest: "abc123".to_string(),
            label: Some("cargo test".to_string()),
        });
        round_trip(EventBody::ToolStarted {
            run_id,
            tool: "shell.run".to_string(),
            args_digest: "abc123".to_string(),
            label: None,
        });
        round_trip(EventBody::ToolCompleted {
            run_id,
            tool: "shell.run".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: Some(artifact_ref()),
        });
        round_trip(EventBody::ToolCompleted {
            run_id,
            tool: "workspace.read_file".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: None,
        });
        round_trip(EventBody::PatchProposed {
            run_id,
            changeset_id: ChangeSetId::new(),
            artifact: artifact_ref(),
            files: vec!["src/lib.rs".to_string()],
            additions: 3,
            deletions: 1,
            preview: "@@ -1 +1 @@\n-old\n+new".to_string(),
            preview_truncated: false,
        });
        round_trip(EventBody::ApprovalRequested {
            approval_id: ApprovalId::new(),
            action: ProposedAction::GitCommit {
                repository: "acme/widget".to_string(),
            },
            risk: Risk {
                level: RiskLevel::Medium,
                reasons: vec![],
            },
            pattern: None,
        });
        round_trip(EventBody::ApprovalResolved {
            approval_id: ApprovalId::new(),
            decision: ApprovalDecision::Approve,
        });
        round_trip(EventBody::SteeringQueued { run_id });
        round_trip(EventBody::SteeringApplied { run_id });
        round_trip(EventBody::BudgetWarning {
            run_id,
            dimension: BudgetDimension::Tokens,
            used: 90_000,
            limit: 100_000,
        });
        round_trip(EventBody::ContextUsage {
            run_id,
            used_tokens: 42_000,
            window_tokens: 200_000,
            system_tokens: 900,
            tool_tokens: 3_100,
            transcript_tokens: 38_000,
        });
        round_trip(EventBody::RunCompleted {
            run_id,
            disposition: RunDisposition::Completed {
                summary: Some("fixed".to_string()),
            },
            chronicle: artifact_ref(),
        });
        round_trip(EventBody::RunUsage {
            run_id,
            prompt_tokens: Some(1002),
            completion_tokens: Some(60),
            cost_micros: None,
            coverage: None,
        });
        round_trip(EventBody::LearningsCaptured {
            run_id: RunId::new(),
            proposed_count: 1,
            proposed_ids: vec![LearningId::new()],
            activated_count: 1,
            activated_ids: vec![LearningId::new()],
        });
        round_trip(EventBody::ClientPresenceChanged {
            client_id: crate::ids::ClientId::new(),
            role: crate::handshake::ClientRole::Contributor,
            present: true,
        });
    }

    #[test]
    fn tool_completed_omits_absent_artifact() {
        let event = event_with(EventBody::ToolCompleted {
            run_id: RunId::new(),
            tool: "workspace.search".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: None,
        });
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(!json.contains("artifact"));
    }

    #[test]
    fn tool_started_omits_absent_label() {
        let event = event_with(EventBody::ToolStarted {
            run_id: RunId::new(),
            tool: "shell.run".to_string(),
            args_digest: "abc123".to_string(),
            label: None,
        });
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(!json.contains("label"));
    }

    #[test]
    fn tool_started_carries_a_present_label() {
        let run_id = RunId::new();
        let event = event_with(EventBody::ToolStarted {
            run_id,
            tool: "workspace.read_file".to_string(),
            args_digest: "abc123".to_string(),
            label: Some("services/main.py".to_string()),
        });
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("services/main.py"));
        let parsed: SessionEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            parsed.body,
            EventBody::ToolStarted {
                run_id,
                tool: "workspace.read_file".to_string(),
                args_digest: "abc123".to_string(),
                label: Some("services/main.py".to_string()),
            }
        );
    }

    /// A `label`-less payload — exactly what an old daemon or an old ledger
    /// entry looks like — must still deserialize, with `label` defaulting to
    /// `None` (the `#[serde(default)]` contract that keeps this an additive,
    /// back-compatible wire change).
    #[test]
    fn tool_started_without_label_field_deserializes_to_none() {
        let json = r#"{"sequence":1,"occurred_at":"2026-01-01T00:00:00Z","actor":{"type":"System"},"body":{"type":"ToolStarted","run_id":"30000000-0000-0000-0000-000000000001","tool":"shell.run","args_digest":"abc123"}}"#;
        let parsed: SessionEvent = serde_json::from_str(json).expect("old payload must parse");
        match parsed.body {
            EventBody::ToolStarted { label, .. } => assert_eq!(label, None),
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    /// An unmeasured dimension must be ABSENT on the wire, never `0` — a zero
    /// would render as "this run was free" in a client that shows a cost chip.
    #[test]
    fn run_usage_omits_unmeasured_dimensions() {
        let event = event_with(EventBody::RunUsage {
            run_id: RunId::new(),
            prompt_tokens: Some(1002),
            completion_tokens: Some(60),
            cost_micros: None,
            coverage: None,
        });
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("prompt_tokens"));
        assert!(!json.contains("cost_micros"));
    }

    /// A run the daemon could measure nothing about still round-trips, and
    /// serializes to the bare tag — the shape an older client sees as
    /// `Unknown` and a newer one renders as "no measurement".
    #[test]
    fn run_usage_with_nothing_measured_carries_only_its_run() {
        let run_id = RunId::new();
        let json = serde_json::to_string(&event_with(EventBody::RunUsage {
            run_id,
            prompt_tokens: None,
            completion_tokens: None,
            cost_micros: None,
            coverage: None,
        }))
        .expect("serialize");
        let parsed: SessionEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            parsed.body,
            EventBody::RunUsage {
                run_id,
                prompt_tokens: None,
                completion_tokens: None,
                cost_micros: None,
                coverage: None,
            }
        );
    }

    #[test]
    fn unknown_event_tag_deserializes_to_unknown() {
        // Mirror the Phase 0 `Payload` unknown-tag test at the event layer: a
        // future event type must deserialize to `Unknown`, not error the frame.
        let mut value = serde_json::to_value(event_with(EventBody::SessionClosed)).expect("value");
        value["body"] = serde_json::json!({ "type": "QuantumEvent", "spin": "up" });
        let parsed: SessionEvent =
            serde_json::from_value(value).expect("future events must parse, not error");
        assert!(matches!(parsed.body, EventBody::Unknown));
    }

    /// The exact Phase 0 bytes from `crates/test-support/fixtures/events-basic.jsonl`.
    /// Embedded as a literal so this test never depends on the test-support crate
    /// (that would create a dependency cycle). Old event bytes must parse forever.
    const PHASE0_FIXTURE_JSONL: &str = r#"{"sequence":1,"occurred_at":"2026-07-14T09:00:00Z","actor":{"type":"System"},"body":{"type":"SessionCreated","title":"fixture session"}}
{"sequence":2,"occurred_at":"2026-07-14T09:00:05Z","actor":{"type":"Human","user_id":"dana"},"body":{"type":"NoteAppended","text":"first note"}}
{"sequence":3,"occurred_at":"2026-07-14T09:00:10Z","actor":{"type":"Human","user_id":"dana"},"body":{"type":"NoteAppended","text":"second note"}}
{"sequence":4,"occurred_at":"2026-07-14T09:00:15Z","actor":{"type":"System"},"body":{"type":"SessionClosed"}}"#;

    #[test]
    fn phase0_fixture_bytes_still_deserialize() {
        let events: Vec<SessionEvent> = PHASE0_FIXTURE_JSONL
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("Phase 0 event must parse forever"))
            .collect();

        assert_eq!(events.len(), 4);
        assert_eq!(events[0].sequence, 1);
        assert!(matches!(events[0].body, EventBody::SessionCreated { .. }));
        assert!(matches!(events[1].body, EventBody::NoteAppended { .. }));
        assert!(matches!(events[2].body, EventBody::NoteAppended { .. }));
        assert!(matches!(events[3].body, EventBody::SessionClosed));
        assert!(matches!(events[3].actor, Actor::System));
    }

    #[test]
    fn question_events_round_trip() {
        let question_id = QuestionId::new();
        let run_id = RunId::new();
        let questions = vec![QuestionPrompt {
            question: "Confirm deployment?".to_string(),
            header: "Deploy".to_string(),
            options: vec![
                QuestionOption {
                    label: "Yes (Recommended)".to_string(),
                    description: "Deploy to production".to_string(),
                },
                QuestionOption {
                    label: "No".to_string(),
                    description: "Cancel".to_string(),
                },
            ],
            multiple: false,
            custom: true,
        }];

        let ask_event = event_with(EventBody::QuestionAsked {
            question_id,
            run_id,
            questions: questions.clone(),
        });
        let json_ask = serde_json::to_string(&ask_event).expect("serialize ask");
        let parsed_ask: SessionEvent = serde_json::from_str(&json_ask).expect("deserialize ask");
        assert_eq!(
            parsed_ask.body,
            EventBody::QuestionAsked {
                question_id,
                run_id,
                questions
            }
        );

        let resolve_event = event_with(EventBody::QuestionResolved {
            question_id,
            outcome: QuestionOutcome::Answered {
                answers: vec![vec!["Yes (Recommended)".to_string()]],
            },
        });
        let json_res = serde_json::to_string(&resolve_event).expect("serialize resolve");
        let parsed_res: SessionEvent =
            serde_json::from_str(&json_res).expect("deserialize resolve");
        assert_eq!(parsed_res.body, resolve_event.body);
    }

    #[test]
    fn checkpoint_events_round_trip() {
        let run_id = RunId::new();
        let checkpoint_id = CheckpointId::new();

        let recorded = event_with(EventBody::CheckpointRecorded {
            run_id,
            checkpoint_id,
            ordinal: 1,
            kind: CheckpointKind::Stash,
            commit: "a".repeat(40),
            base_commit: "b".repeat(40),
        });
        let json_rec = serde_json::to_string(&recorded).expect("serialize recorded");
        let parsed_rec: SessionEvent =
            serde_json::from_str(&json_rec).expect("deserialize recorded");
        assert_eq!(parsed_rec.body, recorded.body);

        let restored = event_with(EventBody::CheckpointRestored {
            run_id,
            checkpoint_id,
            restored: true,
        });
        let json_res = serde_json::to_string(&restored).expect("serialize restored");
        let parsed_res: SessionEvent =
            serde_json::from_str(&json_res).expect("deserialize restored");
        assert_eq!(parsed_res.body, restored.body);
    }

    #[test]
    fn session_forked_event_round_trip() {
        let from_session = SessionId::new();
        let checkpoint = CheckpointId::new();

        let event = event_with(EventBody::SessionForked {
            from_session,
            checkpoint,
        });
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: SessionEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.body, event.body);
    }

    #[test]
    fn pending_prompts_changed_event_round_trip() {
        use crate::ids::PromptId;
        use crate::run::PromptDelivery;

        let event = event_with(EventBody::PendingPromptsChanged {
            prompts: vec![
                PendingPromptView {
                    id: PromptId::new(),
                    text: "steer this".to_string(),
                    mode: AgentMode::Build,
                    delivery: PromptDelivery::Steer,
                },
                PendingPromptView {
                    id: PromptId::new(),
                    text: "queue that".to_string(),
                    mode: AgentMode::Explore,
                    delivery: PromptDelivery::Queue,
                },
            ],
        });
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: SessionEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.body, event.body);
    }
}
