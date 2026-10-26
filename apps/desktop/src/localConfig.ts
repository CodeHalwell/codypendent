/**
 * Types for the two desktop surfaces that are LOCAL CONFIGURATION, not protocol.
 *
 * Neither of these crosses the daemon wire as a command:
 *
 * - **Councils** live in `<config_dir>/councils.toml` (definitions) and
 *   `<data_dir>/councils/<name>/*.json|.md` (results). There is no `Council`
 *   variant in `CommandBody`; the shell reaches them through the shared
 *   `codypendent-council` crate, exactly as the TUI does. A council *run* does
 *   convene real daemon sessions — one per member plus the chair — but it does
 *   so over its own connections, not this client's transcript.
 * - **The repository** is a local choice that then rides on
 *   `CreateSession`/`AttachSession`/`StartRun` as their `repository` field.
 *
 * Every shape here mirrors a Rust type in `src-tauri/src/council.rs` or
 * `src-tauri/src/repository.rs`, which serialize camelCase.
 */

/**
 * A validated repository checkout.
 *
 * `path` is the git toplevel — never the folder the operator happened to click,
 * which may be a subdirectory. `picked` records that folder when it differed, so
 * "I chose `repo/src` and it says `repo`" is explained rather than surprising.
 */
export type RepositorySelection = {
  path: string;
  name: string;
  picked?: string;
};

export type CouncilMemberRow = {
  model: string;
  role: string;
};

/**
 * One configured council.
 *
 * `quorum` is the definition's own explicit value and is absent when it has
 * none; `requiredQuorum` is what the council crate will actually enforce (a
 * simple majority by default). Two fields, because collapsing them would print
 * a number the operator never chose as though they had.
 */
export type CouncilCard = {
  name: string;
  description: string;
  chair: string;
  rounds: number;
  evidence: boolean;
  quorum?: number | null;
  requiredQuorum: number;
  chairIsMember: boolean;
  members: CouncilMemberRow[];
};

/**
 * What the builder submits.
 *
 * Deliberately the same fields the TUI's wizard collects and no more: that
 * wizard has no quorum step and no evidence step, so neither is invented here.
 */
export type CouncilDraft = {
  name: string;
  description: string;
  chair: string;
  rounds: number;
  members: CouncilMemberRow[];
};

/**
 * One member's (or the chair's) completed run inside a durable report.
 *
 * `tokens` and `costMicros` are ABSENT when that run measured nothing. They are
 * never `0` for unknown — a council that reported no usage must not read as a
 * council that cost nothing.
 */
export type CouncilMemberOutcome = {
  model: string;
  role: string;
  sessionId: string;
  runId: string;
  response: string;
  tokens?: number | null;
  costMicros?: number | null;
};

export type CouncilRoundCard = {
  round: number;
  members: CouncilMemberOutcome[];
  /** Every failure reason from this round — a partial run is never a total loss. */
  failures: string[];
};

/** A durable council result, exactly as it was persisted. */
export type CouncilResultCard = {
  resultId: string;
  /** `completed` | `quorum-failed` | `chair-failed` | `runtime-failed`. */
  status: string;
  council: string;
  objective: string;
  startedAt: string;
  finishedAt: string;
  repository: string;
  originSessionId?: string | null;
  evidence: boolean;
  warnings: string[];
  rounds: CouncilRoundCard[];
  failure?: string | null;
  /**
   * The chair's synthesis. Empty when the chair never ran (a quorum failure);
   * `status` and `failure` say which, so an empty synthesis is never mistaken
   * for a chair that answered with nothing.
   */
  synthesis: string;
  /** The independent verdict, when a reviewer actually ran. */
  review?: CouncilMemberOutcome | null;
  participants: string[];
  costLine: string;
  reportMarkdown: string;
};

/**
 * The results browser's page.
 *
 * `warnings` carries per-council read failures, so one unreadable report
 * degrades that row instead of emptying the page. An empty `results` with no
 * warnings means exactly "no council has ever run" — a read that succeeded.
 */
export type CouncilResultsPage = {
  results: CouncilResultCard[];
  warnings: string[];
};

/** One streamed line from a running council. */
export type CouncilProgressFrame = {
  council: string;
  resultId: string;
  /** `round-started` | `member-completed` | `member-failed` | `chair-started` | `warning`. */
  phase: string;
  occurredAt: string;
  message: string;
  activeSubagents: number;
};

/**
 * What a finished council run hands back.
 *
 * EVERY persisted outcome comes back as a `result`, including a quorum or chair
 * failure — the council crate writes a report for those too, and discarding it
 * would throw away the members' completed work. `failure` is set exactly when
 * the run did not complete, so a partial report can never read as a whole one.
 */
export type CouncilRunReply = {
  result?: CouncilResultCard | null;
  failure?: string | null;
};
