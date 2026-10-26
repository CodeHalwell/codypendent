/**
 * Generated from the authoritative Rust protocol schema.
 * Do not edit by hand; run `npm run generate`.
 */

export type Actor =
  | {
      type: "Human";
      user_id: string;
    }
  | {
      agent_id: string;
      model: string;
      run_id: string;
      type: "Agent";
    }
  | {
      client_id: string;
      type: "Client";
    }
  | {
      integration_id: string;
      type: "Integration";
    }
  | {
      type: "System";
    }
  | {
      type: "Unknown";
    };
/**
 * The body of a persisted event.
 *
 * Internally tagged with a `#[serde(other)] Unknown` fallback (RULE 1): an event type produced by a newer daemon deserializes to `Unknown` in an older client instead of failing the whole frame, and the client renders an "unsupported item" placeholder. Phase 0 variants are preserved so old ledger bytes parse forever.
 */
export type EventBody =
  | {
      title: string;
      type: "SessionCreated";
    }
  | {
      /**
       * The run this note belongs to, when it is run-scoped (a run's context manifest or a curated-memory note). `None` for a session-level note (e.g. user input, an effect-reconciliation record), which a client attaches to whatever run is in focus. Without this, a run's note could land on the wrong transcript when runs interleave (issue #6 item 3). `#[serde(default)]` keeps old ledger bytes (which have no `run_id`) parsing to `None`.
       */
      run_id?: string | null;
      text: string;
      type: "NoteAppended";
    }
  | {
      type: "SessionClosed";
    }
  | {
      mode: AgentMode;
      objective: string;
      run_id: string;
      type: "RunStarted";
    }
  | {
      run_id: string;
      state: RunState;
      type: "RunStateChanged";
    }
  | {
      run_id: string;
      text: string;
      /**
       * `true` when this chunk is reasoning, not reply. Defaults to `false` so a payload written before this field existed still parses.
       */
      thought?: boolean;
      type: "ModelStreamDelta";
    }
  | {
      attempt: number;
      /**
       * The wait before the retry fires, in milliseconds.
       */
      delay_ms: number;
      max_attempts: number;
      /**
       * Bounded classifier reason (e.g. "provider is overloaded").
       */
      message: string;
      run_id: string;
      type: "ModelRetrying";
    }
  | {
      action: ProposedAction;
      approval_id: string;
      run_id: string;
      type: "ToolProposed";
    }
  | {
      action: ProposedAction;
      reasons?: string[];
      run_id: string;
      type: "ToolDenied";
    }
  | {
      /**
       * Digest of the tool arguments (not the arguments themselves).
       */
      args_digest: string;
      /**
       * A short, human-readable display label for the call — e.g. the file path a `workspace.read_file` targets, or the command a `shell.run` executes — so a client can render `workspace.read_file · services/main.py` instead of the bare tool name. Derived by the emitter (`codypendent_runtime::tools::tool_label`) from the same arguments `args_digest` hashes, BEFORE they are discarded: bounded, single-line, and never the full arguments or file contents. `#[serde(default)]` keeps old ledger bytes and an older daemon's events (neither carries this field) deserializing to `None` — additive and back-compatible.
       */
      label?: string | null;
      run_id: string;
      /**
       * Tool name, e.g. `shell.run`.
       */
      tool: string;
      type: "ToolStarted";
    }
  | {
      label?: string | null;
      run_id: string;
      tool: string;
      type: "ToolUpdated";
    }
  | {
      /**
       * Bulk output, if any, as an artifact reference.
       */
      artifact?: ArtifactRef | null;
      outcome: ToolOutcome;
      run_id: string;
      tool: string;
      type: "ToolCompleted";
    }
  | {
      /**
       * Added lines in the unified diff.
       */
      additions?: number;
      /**
       * The patch/diff, stored as an artifact.
       */
      artifact: ArtifactRef;
      changeset_id: string;
      /**
       * Removed lines in the unified diff.
       */
      deletions?: number;
      /**
       * Repository-relative paths touched by the change set.
       */
      files?: string[];
      /**
       * A bounded unified-diff preview for immediate review in clients.
       */
      preview?: string;
      /**
       * Whether the full artifact contains more diff than `preview`.
       */
      preview_truncated?: boolean;
      run_id: string;
      type: "PatchProposed";
    }
  | {
      action: ProposedAction;
      approval_id: string;
      pattern?: string | null;
      risk: Risk;
      type: "ApprovalRequested";
    }
  | {
      approval_id: string;
      decision: ApprovalDecision;
      type: "ApprovalResolved";
    }
  | {
      run_id: string;
      type: "SteeringQueued";
    }
  | {
      run_id: string;
      type: "SteeringApplied";
    }
  | {
      dimension: BudgetDimension;
      limit: number;
      run_id: string;
      type: "BudgetWarning";
      used: number;
    }
  | {
      run_id: string;
      system_tokens: number;
      tool_tokens: number;
      transcript_tokens: number;
      type: "ContextUsage";
      used_tokens: number;
      window_tokens: number;
    }
  | {
      /**
       * The run chronicle, stored as a JSON artifact.
       */
      chronicle: ArtifactRef;
      disposition: RunDisposition;
      run_id: string;
      type: "RunCompleted";
    }
  | {
      completion_tokens?: number | null;
      cost_micros?: number | null;
      coverage?: UsageCoverage | null;
      prompt_tokens?: number | null;
      run_id: string;
      type: "RunUsage";
    }
  | {
      activated_count: number;
      activated_ids?: string[];
      proposed_count: number;
      proposed_ids?: string[];
      run_id: string;
      type: "LearningsCaptured";
    }
  | {
      client_id: string;
      /**
       * `true` when the client attached, `false` when it detached.
       */
      present: boolean;
      role: ClientRole;
      type: "ClientPresenceChanged";
    }
  | {
      question_id: string;
      questions: QuestionPrompt[];
      run_id: string;
      type: "QuestionAsked";
    }
  | {
      outcome: QuestionOutcome;
      question_id: string;
      type: "QuestionResolved";
    }
  | {
      /**
       * The commit the run's worktree was carved from — the "state before this turn" restore/fork target for a `commit`-kind checkpoint.
       */
      base_commit: string;
      checkpoint_id: string;
      commit: string;
      kind: CheckpointKind;
      ordinal: number;
      run_id: string;
      type: "CheckpointRecorded";
    }
  | {
      checkpoint_id: string;
      restored: boolean;
      run_id: string;
      type: "CheckpointRestored";
    }
  | {
      checkpoint: string;
      from_session: string;
      type: "SessionForked";
    }
  | {
      prompts: PendingPromptView[];
      type: "PendingPromptsChanged";
    }
  | {
      type: "Unknown";
    };
/**
 * A mode preset: a bundle of policy and interaction defaults, not merely a prompt (Chapter 20). Modes are enforced by the policy engine — an `Explore` run proposing a write is denied regardless of what the model says.
 */
type AgentMode =
  | {
      type: "Ask";
    }
  | {
      type: "Explore";
    }
  | {
      type: "Plan";
    }
  | {
      type: "Build";
    }
  | {
      type: "Review";
    }
  | {
      type: "Unknown";
    };
/**
 * The lifecycle state of a run (Chapter 04). Transitions are persisted before they are exposed to clients.
 */
type RunState =
  | {
      type: "Queued";
    }
  | {
      type: "Preparing";
    }
  | {
      type: "Running";
    }
  | {
      type: "WaitingForApproval";
    }
  | {
      type: "WaitingForUserInput";
    }
  | {
      type: "Paused";
    }
  | {
      type: "Recovering";
    }
  | {
      type: "Completed";
    }
  | {
      type: "Failed";
    }
  | {
      type: "Cancelled";
    }
  | {
      type: "Unknown";
    };
/**
 * A side-effecting action an agent proposes, pending policy evaluation and possibly approval.
 *
 * This started as the Phase 1 minimal subset of the Chapter 14 shape; Phase 3 adds `GitHubMutation` for remote GitHub writes. Further variants (`InstallPlugin`, structured `CommandRequest` / `NetworkDestination`) arrive in later phases. Paths and destinations are carried as strings on the wire.
 */
type ProposedAction =
  | {
      paths: string[];
      type: "ReadFiles";
    }
  | {
      patch: string;
      type: "WritePatch";
    }
  | {
      args: string[];
      /**
       * The working directory the command runs in, when constrained.
       */
      cwd?: string | null;
      /**
       * The child's *complete* environment as name/value pairs (empty means it inherits nothing). Carried on the action so the approver and the audit ledger see exactly what the command runs with: an unshown, model-controlled environment could otherwise smuggle execution-hijacking variables (`LD_PRELOAD`, `RUSTC_WRAPPER`, a shadowed `PATH`, …) past a benign-looking `run cargo test` approval. Defaulted so an older client that sends none still parses.
       */
      environment?: [string, string][];
      program: string;
      type: "ExecuteCommand";
    }
  | {
      destination: string;
      type: "NetworkRequest";
    }
  | {
      repository: string;
      type: "GitCommit";
    }
  | {
      branch: string;
      remote: string;
      type: "GitPush";
    }
  | {
      /**
       * The `owner/repo` slug the mutation targets.
       */
      repository: string;
      /**
       * A short human-readable description of the write, rendered on the approval card (e.g. `create draft PR on owner/repo`).
       */
      summary: string;
      type: "GitHubMutation";
    }
  | {
      /**
       * The repo-relative files the publish changes.
       */
      changed_files: string[];
      document_id: string;
      /**
       * The resulting Git action (e.g. `commit docs/x.md on branch docs/publish`).
       */
      git_action: string;
      /**
       * A short human description of the target (e.g. `repository file docs/architecture.md`).
       */
      target: string;
      type: "PublishDocument";
    }
  | {
      /**
       * The artifact kind being posted (`finding`, `decision`, …).
       */
      kind: string;
      type: "BlackboardPost";
      /**
       * The workflow run whose board is written (server-derived from the run context, never model-supplied).
       */
      workflow_run_id: string;
    }
  | {
      type: "BlackboardQuery";
      /**
       * The workflow run whose board is read (server-derived).
       */
      workflow_run_id: string;
    }
  | {
      /**
       * The model-supplied arguments as canonical JSON text (a `String`, not a `Value`, so the enum stays `Eq` and the digest is stable).
       */
      args: string;
      /**
       * The server name from `mcp.toml` (server-derived from the tool name's `mcp.<server>.<tool>` prefix, never free-form model text).
       */
      server: string;
      /**
       * A short human-readable description of the call, rendered on the approval card (e.g. `github.create_issue("…")`).
       */
      summary: string;
      /**
       * The tool name on that server (from the server's `tools/list`).
       */
      tool: string;
      type: "McpToolCall";
    }
  | {
      /**
       * Official registry id of the connected agent.
       */
      agent: string;
      /**
       * Canonical, bounded ACP tool-call description.
       */
      details: string;
      /**
       * Human-readable tool/call title reported by ACP.
       */
      title: string;
      type: "AcpToolCall";
    }
  | {
      type: "RecordMemory";
    }
  | {
      type: "SearchRegistry";
    }
  | {
      /**
       * The document the tool call targets; empty for `docs.create` (the document does not exist yet) and `docs.read` listings.
       */
      document_id: string;
      /**
       * A short human description of the access (e.g. `docs.edit block p`), for the trace.
       */
      summary: string;
      type: "DocumentEdit";
    }
  | {
      type: "WorkflowQuery";
      /**
       * The workflow run being read, or empty when listing the repository's runs (server-derived from the run context / validated args).
       */
      workflow_run_id: string;
    }
  | {
      summary: string;
      type: "WorkflowCreate";
      workflow_id: string;
    }
  | {
      /**
       * `named` or `inline`, so approval surfaces distinguish persistence from an ephemeral manifest without decoding tool arguments.
       */
      kind: string;
      summary: string;
      type: "WorkflowRun";
      workflow_id: string;
    }
  | {
      /**
       * The canonical repository whose board is written (server-derived from the run context, never model-supplied).
       */
      repository: string;
      /**
       * A short human rendering of the write (e.g. `create "wire the DAG"`).
       */
      summary: string;
      type: "TaskWrite";
    }
  | {
      /**
       * The canonical repository whose board is read (server-derived).
       */
      repository: string;
      type: "TaskRead";
    }
  | {
      name: string;
      summary: string;
      type: "CouncilCreate";
    }
  | {
      name: string;
      summary: string;
      type: "CouncilRun";
    }
  | {
      selector: string;
      type: "CouncilResultRead";
    }
  | {
      /**
       * The canonical repository whose graph is read (server-derived from the run context, never model-supplied).
       */
      repository: string;
      /**
       * A short human rendering of the question (e.g. `callers of Router::decide`), for the trace.
       */
      summary: string;
      type: "CodeGraphQuery";
    }
  | {
      /**
       * The canonical repository whose graph is written (server-derived from the run context, never model-supplied).
       */
      repository: string;
      /**
       * A short human rendering of the assertion (e.g. `assert handle_charge calls ChargeService::run`), for the trace.
       */
      summary: string;
      type: "CodeGraphAssert";
    }
  | {
      /**
       * The bounded `header` of each question, for the trace.
       */
      headers: string[];
      /**
       * How many questions the call carries.
       */
      question_count: number;
      type: "AskUser";
    }
  | {
      /**
       * The checkpoint commit being restored to.
       */
      commit: string;
      /**
       * The checkpoint's turn ordinal within the run.
       */
      ordinal: number;
      /**
       * The run whose worktree is rewound (string form of the RunId).
       */
      run_id: string;
      type: "RestoreCheckpoint";
      /**
       * The worktree directory the reset/clean/apply will run in.
       */
      worktree: string;
    }
  | {
      /**
       * The number of stdin bytes written (the payload itself is never carried, so no echoed secret reaches the ledger).
       */
      byte_len: number;
      /**
       * The id of the already-running process whose stdin is written (server-tracked; the model names an existing process, never spawns one here).
       */
      process_id: number;
      type: "WriteProcessStdin";
    }
  | {
      /**
       * The mode the accepted continuation runs in (`Plan` from `plan_enter`, `Build` from `plan_exit`).
       */
      target: AgentMode;
      type: "PlanTransition";
    }
  | {
      name: string;
      type: "ReadSecret";
    }
  | {
      type: "Unknown";
    };
/**
 * How sensitive an artifact's contents are.
 *
 * Ordered least to most restrictive; higher classifications gate model routing, export, and display. A wire enum, so it is internally tagged and carries an [`DataClassification::Unknown`] fallback for forward compatibility.
 */
type DataClassification =
  | {
      type: "Public";
    }
  | {
      type: "Internal";
    }
  | {
      type: "Confidential";
    }
  | {
      type: "Secret";
    }
  | {
      type: "Unknown";
    };
/**
 * The outcome of a completed tool call, carried by `ToolCompleted`.
 *
 * Chapter 03 lists tool-completed as an event category without fixing its payload; this is the minimal reasonable shape — success, or failure with a short message. Bulk output travels as an `ArtifactRef`, never here.
 */
type ToolOutcome =
  | {
      type: "Succeeded";
    }
  | {
      message: string;
      type: "Failed";
    }
  | {
      type: "Unknown";
    };
/**
 * Severity buckets for a [`Risk`].
 */
type RiskLevel =
  | {
      type: "Low";
    }
  | {
      type: "Medium";
    }
  | {
      type: "High";
    }
  | {
      type: "Critical";
    }
  | {
      type: "Unknown";
    };
/**
 * The decision an approver returns for a proposed action.
 */
type ApprovalDecision =
  | {
      type: "Approve";
    }
  | {
      type: "Reject";
    }
  | {
      type: "Unknown";
    };
/**
 * Which budget a `BudgetWarning` is about. The unit of the reported `used`/`limit` is implied by the dimension (tokens, minor currency units, seconds, or a count of calls).
 */
type BudgetDimension =
  | {
      type: "Tokens";
    }
  | {
      type: "Cost";
    }
  | {
      type: "WallClock";
    }
  | {
      type: "ToolCalls";
    }
  | {
      type: "Unknown";
    };
/**
 * The terminal outcome of a run, carried by `RunCompleted`.
 *
 * Chapter 04 names the terminal `RunState`s but leaves the disposition detail open at Phase 1; this is the minimal reasonable shape — the terminal kind plus a short human-readable summary or reason.
 */
type RunDisposition =
  | {
      summary?: string | null;
      type: "Completed";
    }
  | {
      /**
       * The structured half of the failure: a stable code, whether a retry could succeed, and the affordance a client should offer — derived from the typed error where one exists, never from `reason`'s text. Absent from older daemons and from failures nothing has classified; a client reads it first and falls back to `reason`.
       */
      error?: CodypendentError | null;
      reason: string;
      type: "Failed";
    }
  | {
      reason?: string | null;
      type: "Cancelled";
    }
  | {
      type: "Unknown";
    };
/**
 * A machine-readable hint about how the user could resolve an error, so the client can render the right affordance instead of parsing `message`.
 */
type UserAction =
  | {
      type: "Retry";
    }
  | {
      type: "Reauthenticate";
    }
  | {
      type: "GrantApproval";
    }
  | {
      type: "AdjustPolicy";
    }
  | {
      type: "ReconfigureModel";
    }
  | {
      type: "ContactSupport";
    }
  | {
      type: "Unknown";
    };
/**
 * A client's authority over a session it observes (Chapter 03). Exclusivity is attached to specific resources (leases), not to the whole session.
 */
type ClientRole =
  | {
      type: "Observer";
    }
  | {
      type: "Contributor";
    }
  | {
      type: "Controller";
    }
  | {
      type: "Approver";
    }
  | {
      type: "Unknown";
    };
/**
 * How a question was resolved.
 */
type QuestionOutcome =
  | {
      answers: string[][];
      type: "Answered";
    }
  | {
      feedback?: string | null;
      type: "Rejected";
    }
  | {
      type: "Unknown";
    };
/**
 * How a filesystem checkpoint is materialized (Adoption 04).
 */
type CheckpointKind =
  | {
      type: "Stash";
    }
  | {
      type: "Commit";
    }
  | {
      type: "Unknown";
    };
/**
 * How a pending prompt is delivered (Adoption 06, cline's `PendingPromptDelivery`). `Steer` feeds the live run's steering channel at its next safe point; `Queue` waits for the session to go idle and launches a continuation run.
 */
type PromptDelivery =
  | {
      type: "Queue";
    }
  | {
      type: "Steer";
    }
  | {
      type: "Unknown";
    };

export interface SessionEvent {
  actor: Actor;
  body: EventBody;
  causation_id?: string | null;
  correlation_id?: string | null;
  occurred_at: string;
  sequence: number;
}
/**
 * A pointer to a stored artifact plus the metadata needed to handle it safely.
 *
 * `id` and `sha256` are deliberately independent: identical bytes dedup to one blob (keyed by `sha256`) but every occurrence is its own `ArtifactRef` with its own id and `sensitivity` (Chapter 14 / STEP 1.4). Classification checks always read the ref in hand, never a row looked up by hash.
 */
interface ArtifactRef {
  byte_length: number;
  id: string;
  /**
   * IANA media type, e.g. `text/plain` or `application/json`.
   */
  media_type: string;
  sensitivity: DataClassification;
  /**
   * Lowercase hex SHA-256 of the blob's bytes (the content address).
   */
  sha256: string;
}
/**
 * A structured risk assessment attached to a proposed action or approval request. Chapter 14 leaves the exact shape open at Phase 1; this is the minimal reasonable form — a severity level plus human-readable reasons.
 */
interface Risk {
  level: RiskLevel;
  reasons?: string[];
}
/**
 * A machine-readable, correlated error.
 *
 * `code` is a stable dotted identifier (for example `protocol.unsupported-payload` or `policy.write-denied`) that receivers branch on; `message` is for humans only.
 */
interface CodypendentError {
  /**
   * Stable machine-readable code. Never parse `message` to decide behaviour.
   */
  code: string;
  correlation_id: string;
  details?: JsonValue;
  /**
   * Human-readable explanation.
   */
  message: string;
  /**
   * Whether an identical retry could succeed.
   */
  retryable: boolean;
  /**
   * A suggested next step the client can surface as an affordance.
   */
  user_action?: UserAction | null;
}
/**
 * Completeness of the measured subtotal. Requests count calls to the model driver, including failed/cancelled calls; each measured dimension counts the calls that reported it. Retries without their own usage leave the subtotal partial even when the final attempt reports a measurement. Absent metadata on older events means coverage is unknown.
 */
export interface UsageCoverage {
  completion_requests: number;
  cost_requests: number;
  prompt_requests: number;
  requests: number;
  unmeasured_retries?: number;
}
/**
 * One question as asked. `custom` is carried on the wire but deliberately NOT advertised in the tool schema — the model can never disable free-text answers (opencode's Prompt/Info split).
 */
interface QuestionPrompt {
  /**
   * Allow typing a custom answer (default true).
   */
  custom?: boolean;
  /**
   * Very short label (≤ 30 chars) shown as the card/tab title.
   */
  header: string;
  /**
   * Allow selecting more than one option.
   */
  multiple?: boolean;
  /**
   * Available choices (may be empty only when `custom` is true).
   */
  options: QuestionOption[];
  /**
   * The complete question.
   */
  question: string;
}
/**
 * One selectable choice.
 */
interface QuestionOption {
  /**
   * Explanation of the choice (may be empty).
   */
  description?: string;
  /**
   * Display text (1–5 words, concise).
   */
  label: string;
}
/**
 * One pending prompt, as carried on the `PendingPromptsChanged` snapshot.
 */
interface PendingPromptView {
  delivery: PromptDelivery;
  id: string;
  mode: AgentMode;
  text: string;
}

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
