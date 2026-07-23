//! Application state and its projections (STEP 1.12 RULE 2).
//!
//! [`AppState`] is the single source of truth the renderer reads. It is mutated
//! only by [`crate::reduce::reduce`]; it holds no I/O handles. All state is
//! derived deterministically from the ordered [`SessionEvent`] stream plus local
//! navigation, so replaying the same events yields the same state.

use std::cell::Cell;

use codypendent_protocol::{
    AgentMode, ApprovalId, ArtifactRef, BudgetDimension, ChangeSetId, DocumentId, DocumentMutation,
    ModelId, ProposedAction, Risk, RunDisposition, RunId, RunState, ToolOutcome,
};

use crate::action::Intent;

/// Which pane currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Left pane: the session / run list.
    Sessions,
    /// Center pane: the transcript.
    Transcript,
    /// Right pane: pending approvals + run details.
    Approvals,
}

impl Pane {
    /// The next pane in `Tab` order.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Pane::Sessions => Pane::Transcript,
            Pane::Transcript => Pane::Approvals,
            Pane::Approvals => Pane::Sessions,
        }
    }
}

/// Which base layout the shell renders. Toggled at runtime (`F2` or the palette);
/// the composer and status footer are identical in both — only the region above
/// them changes, and the input model (composer / palette / approval modal) is the
/// same in each, so the panes are at-a-glance context, not a separate mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// The single-column conversation (the Claude Code / Codex feel). Default.
    #[default]
    Chat,
    /// Runs │ conversation │ approvals panes, for at-a-glance workspace state.
    Workspace,
}

impl LayoutMode {
    /// The other layout.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            LayoutMode::Chat => LayoutMode::Workspace,
            LayoutMode::Workspace => LayoutMode::Chat,
        }
    }
}

/// How the input layer should interpret the next key (see
/// [`crate::input::map_event`]). Derived from the active overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// A navigable overlay (Skills / Memory / Docs / Edges / Workflow /
    /// Blackboard / Help) is live: the arrow/command key table drives it.
    Normal,
    /// A text prompt is capturing printable keys.
    Editing,
    /// A yes/no confirmation is awaiting a decision.
    Confirm,
    /// The command palette is capturing a filter query while staying navigable
    /// (printable keys filter; arrows move the selection; Enter runs it).
    Palette,
    /// The base conversation view: the persistent composer captures typed text;
    /// `/` (on an empty composer) opens the palette; Enter sends; PgUp/PgDn
    /// scroll the transcript; Ctrl-↑/↓ switch runs.
    Composer,
    /// A pending approval owns the screen: only the decision keys (`a`/`A`/`r`)
    /// and selection arrows are live, so an approval is never typed past.
    Approval,
}

/// The top-most modal / overlay, if any. Text prompts carry their buffer inline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Overlay {
    /// No overlay; the base layout is interactive.
    #[default]
    None,
    /// The help overlay listing key bindings.
    Help,
    /// The new-run objective prompt (buffer inline).
    NewRun(String),
    /// The steering-text prompt (buffer inline).
    Steering(String),
    /// A "cancel this run?" confirmation.
    ConfirmCancel,
    /// The Skill Studio browser (STEP 2.6): the [`AppState::skills`] list plus a
    /// detail panel that shows the selected skill's description, risk, and its
    /// requested permissions **verbatim** ("skill permissions are visible").
    Skills,
    /// The memory browser (STEP 2.6): the [`AppState::memories`] list plus a
    /// provenance card. `source_open` is whether the focused memory's source has
    /// been revealed by the "open source" affordance — the TUI does no I/O, so
    /// opening surfaces the full source string in place; a real file-open is the
    /// CLI's job later ("every retrieved memory opens its source").
    Memory { source_open: bool },
    /// The Docs Studio browser (Phase 4 client wiring): the [`AppState::docs`]
    /// tree on the left, and the focused document's editor rail (its blocks) +
    /// review rail (its pending suggestions) on the right. Read-only — the live
    /// CRDT edit transport is a separate follow-up.
    Docs,
    /// The code-graph edge inspector (Phase 4 exit criterion 4): the
    /// [`AppState::edges`] list on the left and, for the focused edge, its
    /// relation, confidence, evidence kind + source, and revision on the right.
    Edges,
    /// The workflow-graph view (Phase 5 STEP 5.2, exit criterion 3): the
    /// [`AppState::workflow`] node list on the left and, for the focused node,
    /// its action, state, agent, workspace, approval, retry, dependencies, and
    /// declared outputs on the right. Read-only — a projection of the compiled
    /// workflow graph (the daemon-side executor that fills live per-node
    /// state/cost is a later wiring step).
    Workflow,
    /// The blackboard view (Phase 5 STEP 5.3): the [`AppState::blackboard`] item
    /// list (the typed, attributed artifacts agents share within a workflow run —
    /// findings, decisions, patches, …) grouped by run, and — for the focused
    /// item — its kind, author, confidence, evidence, revision, and payload
    /// summary. Read-only — a projection of the per-run board (populated once the
    /// executor posts artifacts).
    Blackboard,
    /// The command palette: a searchable list of every command the TUI exposes,
    /// so the growing feature set stays reachable without consuming a single-key
    /// binding each. `query` is the live filter; `selected` indexes the filtered
    /// results (reset to 0 whenever the query changes). Opened with `/`.
    Palette { query: String, selected: usize },
    /// The Docs Studio block-edit prompt (Phase 4 STEP 4.3 client wiring): a
    /// single-line buffer for the text to insert into the focused block. On submit
    /// the reducer acquires the block's edit lease and, once granted, sends the
    /// `MutateDocument`; the daemon's collaboration mode decides whether it applies
    /// directly (Edit) or lands as a suggestion (Suggest). `block_id` is the block
    /// the edit targets, captured when the prompt opened.
    DocEdit { block_id: String, buffer: String },
    /// The model picker (MP1): a fuzzy-filterable list of the models
    /// selectable for a run (see [`AppState::models`]), opened from the
    /// command palette's `/model` entry. `query` filters by id/provider
    /// substring; `selected` indexes the filtered results (reset to 0
    /// whenever the query changes) — the same shape as [`Overlay::Palette`].
    /// Marks the model serving the active run as current; `Enter` stages the
    /// focused row on [`AppState::pending_model`] (advisory only this task —
    /// MP2 wires it to actually pin the next run's model).
    ModelPicker { query: String, selected: usize },
}

/// The lifecycle of a single tool card in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    /// Proposed and awaiting policy / approval.
    Proposed,
    /// Executing.
    Running,
    /// Finished (see [`ToolCard::outcome`]).
    Completed,
}

/// A tool invocation rendered as an expandable card.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    /// Tool name, e.g. `shell.run` (empty until [`ToolStarted`] names it).
    ///
    /// [`ToolStarted`]: codypendent_protocol::EventBody::ToolStarted
    pub tool: String,
    pub status: ToolStatus,
    /// The proposed action (present when the card began as a proposal).
    pub action: Option<ProposedAction>,
    /// Digest of the tool arguments (never the arguments themselves).
    pub args_digest: Option<String>,
    /// Terminal outcome, once completed.
    pub outcome: Option<ToolOutcome>,
    /// Bulk output reference, if the tool produced one.
    pub artifact: Option<ArtifactRef>,
    /// The approval this proposal is gated on, if any.
    pub approval_id: Option<ApprovalId>,
    /// Whether the card is expanded to show detail.
    pub expanded: bool,
}

/// A proposed patch / change set rendered as an expandable summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSummary {
    pub changeset_id: ChangeSetId,
    pub artifact: ArtifactRef,
    pub expanded: bool,
}

/// One entry in a run's transcript. Streaming model text is coalesced into a
/// single [`TranscriptEntry::Model`] run; every other event kind is its own
/// entry so it can be selected and expanded independently.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    /// The user's own message — the run objective, or a steering follow-up.
    User { text: String },
    /// Coalesced streamed model prose.
    Model { text: String },
    /// A tool card (boxed: it is by far the largest variant).
    Tool(Box<ToolCard>),
    /// A proposed patch / change set.
    Patch(PatchSummary),
    /// A steering marker.
    Steering { applied: bool },
    /// A budget warning.
    Budget {
        dimension: BudgetDimension,
        used: u64,
        limit: u64,
    },
    /// The run's terminal marker.
    Completed { disposition: RunDisposition },
    /// A note appended to the session. Long/multiline notes fold by default,
    /// mirroring [`ToolCard`]/[`PatchSummary`]; `expanded` is client-only view
    /// state — it is never part of the `NoteAppended` wire event.
    Note { text: String, expanded: bool },
    /// Folded backstage material for the run: the context manifest and
    /// curated-memory writes, which are real but not part of the visible
    /// conversation. Rendered as one dim, expandable line instead of a
    /// visible [`TranscriptEntry::Note`] cell — at most one per run (later
    /// `NoteAppended`s of either kind update the same entry's counts/`raw`
    /// rather than creating another). Entirely client-only view state; never
    /// part of the wire (the underlying `NoteAppended` events are unchanged).
    Backstage {
        /// The most recently seen context manifest's line count, or `None`
        /// if this run has not received one.
        context_lines: Option<usize>,
        /// How many curated-memory (`remembered:`) notes have folded in.
        memory_updates: usize,
        /// The full text of every folded note, in arrival order — revealed
        /// when `expanded`.
        raw: Vec<String>,
        /// Whether the raw bodies are shown below the summary line.
        expanded: bool,
    },
    /// A forward-compatibility placeholder for an event this build does not
    /// understand (protocol RULE 1: render, do not crash).
    Unsupported { label: String },
}

/// A pending approval awaiting a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub approval_id: ApprovalId,
    pub action: ProposedAction,
    pub risk: Risk,
    /// The run this approval belongs to, when it can be inferred (a
    /// `ToolProposed` links an approval to a run; a bare `ApprovalRequested`
    /// does not carry the run id).
    pub run_id: Option<RunId>,
}

/// A run's current derived activity — never fetched, always folded from the
/// event stream (STEP 1.12 RULE 2): the reducer transitions it as it folds
/// run-state, streamed model text, and tool-lifecycle events, so the renderer
/// always has an explanation for a run that would otherwise look paused
/// between transcript updates. Defaults to [`RunActivity::Idle`] (a fresh run
/// has not started preparing yet).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RunActivity {
    /// Not running: queued, paused, awaiting approval/input, or terminal.
    #[default]
    Idle,
    /// Preparing or running, with no model text streaming and no tool in
    /// flight — the agent is composing its next step.
    Thinking,
    /// Model text is actively streaming into the transcript.
    Streaming,
    /// A tool is executing; carries the tool's name.
    RunningTool(String),
}

/// Everything known about one run, and its transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct RunView {
    pub run_id: RunId,
    pub objective: String,
    pub mode: AgentMode,
    pub state: RunState,
    /// The run's derived live-activity status: sets/clears as the reducer
    /// folds run-state, streaming, and tool-lifecycle events; the renderer
    /// shows it as a dim status row so a run is never silently paused with
    /// no explanation.
    pub activity: RunActivity,
    /// The model serving the run, learned from agent-authored events.
    pub model: Option<ModelId>,
    /// The worktree name, once known.
    pub worktree: Option<String>,
    /// Context-window usage percent, projected from the token budget.
    pub context_percent: Option<u16>,
    /// Cost so far, in minor currency units, projected from the cost budget.
    pub cost_minor: Option<u64>,
    pub disposition: Option<RunDisposition>,
    /// The ordered transcript.
    pub transcript: Vec<TranscriptEntry>,
    /// Selected transcript entry (for expand / detail).
    pub transcript_selected: usize,
    /// Transcript scroll offset in rows (used only when not following).
    pub scroll: u16,
    /// Whether the conversation is pinned to the latest content. `true` by
    /// default and after sending; scrolling up with PgUp leaves follow mode, and
    /// paging back to the bottom re-enters it. When following, the renderer shows
    /// the tail of the transcript regardless of `scroll`.
    pub follow: bool,
}

impl RunView {
    fn new(run_id: RunId, objective: String, mode: AgentMode) -> Self {
        Self {
            run_id,
            objective,
            mode,
            state: RunState::Queued,
            activity: RunActivity::Idle,
            model: None,
            worktree: None,
            context_percent: None,
            cost_minor: None,
            disposition: None,
            transcript: Vec::new(),
            transcript_selected: 0,
            scroll: 0,
            follow: true,
        }
    }
}

/// A Skill Studio card (STEP 2.6): one registry item projected for the Skills
/// browser. Self-contained — the TUI never depends on `codypendent-knowledge`;
/// the CLI harness maps each `RegistryItem` into this shape (the one place the
/// two worlds meet). Every field is a pre-rendered human string so the renderer
/// stays a pure projection.
///
/// `permissions` are the requested capabilities rendered **verbatim** (e.g.
/// `"filesystem_read: $REPOSITORY"`, `"command: cargo"`) — the exact strings the
/// package declared, never a paraphrase — so the "skill permissions are visible"
/// exit criterion holds at a glance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCard {
    /// The item's name (its registry identity within a scope).
    pub name: String,
    /// The kind label (`tool` / `skill` / `plugin` / `hook` / `command`).
    pub kind: String,
    /// The scope the item is installed at (e.g. `system`, `workspace …`).
    pub scope: String,
    /// The provenance trust tier (`untrusted` … `first-party`).
    pub trust: String,
    /// The lifecycle status (`draft` / `active` / `modified` / `deprecated`).
    pub status: String,
    /// The coarse risk class (`safe` / `low` / `medium` / `high`).
    pub risk: String,
    /// The item's description (untrusted content; shown, never trusted).
    pub description: String,
    /// The requested capabilities, one verbatim string per capability.
    pub permissions: Vec<String>,
}

/// A memory provenance card (STEP 2.6): one curated memory projected for the
/// Memory browser. Also self-contained — the CLI maps a `MemoryRecord` (via its
/// `ProvenanceCard`) into it. The renderer draws the Chapter 06 provenance card
/// (statement, source, revision, scope, confidence) from these fields alone.
///
/// `source` is a human rendering of the memory's evidence ref (e.g. `"events
/// 3..7 of session <id>"` or `"artifact <ref> (path)"`); the "open source"
/// affordance surfaces it in full so every retrieved memory opens its source.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCard {
    /// The remembered fact.
    pub statement: String,
    /// The memory class (`semantic` / `procedural` / `preference` / …).
    pub class: String,
    /// The scope the memory lives in (cross-repository isolation is enforced in
    /// the store, never here).
    pub scope: String,
    /// The revision the memory is valid from.
    pub revision: String,
    /// When the memory was observed (a date string).
    pub observed: String,
    /// The curator's confidence in the fact, in `[0, 1]`.
    pub confidence: f32,
    /// The human-readable evidence source (what "open source" reveals).
    pub source: String,
}

/// A Docs Studio card (STEP 4.x client wiring): one [`KnowledgeDocument`]
/// projected for the Docs browser's tree/editor/review rails. Self-contained —
/// the TUI never depends on `codypendent-knowledge`; the CLI maps a document
/// snapshot (plus its pending suggestions) into this shape. Every field is a
/// pre-rendered human string so the renderer stays a pure projection.
///
/// [`KnowledgeDocument`]: (mapped by the CLI from `codypendent-knowledge`)
#[derive(Debug, Clone, PartialEq)]
pub struct DocCard {
    /// The document's stable id — the key that correlates an incoming
    /// [`DocumentSync`](codypendent_protocol::DocumentSync) (merged into the
    /// client replica by the CLI harness) back to this card, and the target of an
    /// edit's `MutateDocument`/`AcquireDocumentLease`.
    pub document_id: DocumentId,
    /// The document title (its heading in the tree).
    pub title: String,
    /// The scope the document lives in (e.g. `system`, `workspace …`).
    pub scope: String,
    /// The lifecycle status (`draft` / `in_review` / `published` / `archived`).
    pub status: String,
    /// The collaboration mode governing agent edits (`ask` / `suggest` / `edit`
    /// / `co_author` / `review` / `maintain`) — org docs default to `suggest`.
    pub mode: String,
    /// The document's monotonic revision, pre-rendered (e.g. `"r7"`).
    pub revision: String,
    /// The rendered blocks (the editor rail), in document order.
    pub blocks: Vec<DocBlockView>,
    /// The pending suggestions on the document (the review rail).
    pub suggestions: Vec<DocSuggestionView>,
}

/// One rendered document block (the editor rail). `text` is the block's primary
/// text or a structured-block label — never the raw serialized content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocBlockView {
    /// The block's stable id — the target an edit action leases and mutates (never
    /// rendered; carried so the reducer can name the block without a second lookup).
    pub id: String,
    /// The block kind label (`heading` / `paragraph` / `code` / …).
    pub kind: String,
    /// A one-line human rendering of the block's content.
    pub text: String,
}

/// One pending suggestion on a document (the review rail): a proposed
/// replacement over a character range, with its author and rationale. Rendered
/// read-only — accept/reject is a later live-transport concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSuggestionView {
    /// The suggestion's stable id — the target of an accept/reject
    /// `MutateDocument` (never rendered; carried so the reducer can resolve the
    /// focused suggestion without a second lookup).
    pub id: String,
    /// The suggestion status (`pending` for the review rail).
    pub status: String,
    /// Who proposed it, pre-rendered (e.g. `"agent"` / `"human"`).
    pub author: String,
    /// The character range it targets, pre-rendered (e.g. `"12..40"`).
    pub range: String,
    /// The proposed replacement text.
    pub replacement: String,
    /// The proposer's rationale, when given.
    pub rationale: Option<String>,
}

/// Which rail of the Docs Studio overlay the keyboard drives (Phase 4 client
/// wiring). Defaults to [`DocFocus::Tree`] so the overlay opens on the document
/// list exactly as before this focus existed; `Tab` cycles it, and `↑/↓` then move
/// the selection within the focused rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocFocus {
    /// The document tree (left): `↑/↓` move [`AppState::selected_doc`].
    #[default]
    Tree,
    /// The editor rail (right, top): `↑/↓` move [`AppState::selected_block`]; `e`
    /// edits the focused block.
    Editor,
    /// The review rail (right, bottom): `↑/↓` move [`AppState::selected_suggestion`];
    /// `a`/`r` accept/reject the focused suggestion.
    Review,
}

impl DocFocus {
    /// The next rail in `Tab` order (Tree → Editor → Review → Tree).
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            DocFocus::Tree => DocFocus::Editor,
            DocFocus::Editor => DocFocus::Review,
            DocFocus::Review => DocFocus::Tree,
        }
    }
}

/// The state of the client's edit lease over one document block (Phase 4 client
/// wiring, presence-lite). Surfaced in the Docs editor rail so a writer sees
/// whether it holds the block or is blocked by another writer — the one
/// status-line touch the collaboration slice needs (no cursors/presence UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocLeaseState {
    /// `AcquireDocumentLease` sent; awaiting the grant.
    Acquiring,
    /// The lease is held — edits may apply.
    Held,
    /// The range is leased by another writer (`document.range-leased`).
    Blocked,
}

/// One in-flight document edit: the block being leased/edited, the lease's
/// lifecycle, and the mutation queued until the lease is granted. The reducer
/// stores this on [`AppState::doc_edit`] so the lease→mutate handshake — inherently
/// two round-trips — is driven by folding the daemon's replies, keeping the TUI a
/// pure reducer.
#[derive(Debug, Clone, PartialEq)]
pub struct DocEdit {
    /// The document being edited.
    pub document_id: DocumentId,
    /// The block the lease covers (`None` would be a whole-document structural
    /// lease; the editor only takes block leases).
    pub block_id: Option<String>,
    /// Where the lease is in its lifecycle.
    pub lease: DocLeaseState,
    /// The granted lease id, once held — the capability needed to release it.
    pub lease_id: Option<String>,
    /// The mutation to send once the lease is granted, then cleared (fired once).
    pub pending: Option<DocumentMutation>,
}

/// A code-graph edge projected for the graph-edge inspector (Phase 4 exit
/// criterion 4: "graph edges expose evidence + revision"). Self-contained — the
/// CLI maps a `CodeEdge` (resolving its endpoint node ids to qualified names)
/// into this shape. Every field is a pre-rendered human string.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphEdgeCard {
    /// The source symbol's qualified name (or a fallback id when unresolved).
    pub from: String,
    /// The target symbol's qualified name (or a fallback id when unresolved).
    pub to: String,
    /// The relation label (`calls` / `defines` / `imports` / …).
    pub relation: String,
    /// The edge confidence in `[0, 1]` — the tier its evidence earns.
    pub confidence: f32,
    /// The evidence layer that produced it (`syntax_inferred` / `lsp_resolved`
    /// / `compiler_resolved` / `runtime_observed`).
    pub evidence_kind: String,
    /// A human rendering of the descriptive evidence ref, or `"(none)"`.
    pub evidence: String,
    /// The git revision the edge was recorded at.
    pub revision: String,
}

/// A workflow-graph node projected for the workflow view (Phase 5 STEP 5.2, exit
/// criterion 3: "per-node state, cost, agent, worktree"). Self-contained — the
/// TUI never depends on `codypendent-workflow`; the CLI compiles a
/// `workflow.yaml` manifest and maps each `CompiledNode` (overlaid with the
/// durable node record's state/cost when a run exists) into this shape. Every
/// field is a pre-rendered human string so the renderer stays a pure projection.
///
/// Nodes are listed in the compiled topological order, grouped by their
/// `workflow` label, so the view reads as an ordered graph rather than a flat
/// pile when a repository declares more than one workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowNodeCard {
    /// The owning workflow, pre-rendered (e.g. `"repair-github-check v1"`), so
    /// several workflows can share the list under labeled groups.
    pub workflow: String,
    /// The node (step) id, unique within its workflow.
    pub id: String,
    /// The node's action, pre-rendered (e.g. `"agent implementer · skill
    /// code.repair"` or `"tool repository.test"`).
    pub action: String,
    /// The action kind label (`agent` / `tool`) — drives the list glyph color.
    pub kind: String,
    /// The node's lifecycle state, pre-rendered (`pending` until a durable run
    /// record overlays a live state such as `running` / `completed`).
    pub state: String,
    /// The agent role, when this is an agent node, else `"—"`.
    pub agent: String,
    /// The model-selection policy for an agent node, else `"—"`.
    pub model_policy: String,
    /// How the node's workspace is provisioned (`shared worktree` / `isolated
    /// worktree`) — the exit-criterion "worktree" field.
    pub workspace: String,
    /// The approval policy gating the node (`before write` / `always` / `none`).
    pub approval: String,
    /// The retry policy, pre-rendered (e.g. `"1 attempt"` / `"2 attempts · 5s
    /// backoff"`).
    pub retry: String,
    /// The nodes this one depends on, pre-rendered (comma-joined, or `"—"`).
    pub depends_on: String,
    /// The blackboard artifact kinds the node declares to produce, pre-rendered
    /// (comma-joined, or `"—"`).
    pub outputs: String,
    /// The node's MEASURED cost, pre-rendered (e.g. `"12s · 3 tool calls"`, or
    /// `"—"` until a durable run records one). Only measured dimensions appear —
    /// never a fabricated token/USD figure (Phase 5 T8).
    pub cost: String,
    /// The node's latest failure or budget-block reason when a durable run
    /// recorded one (P5-D4), else `"—"`. Surfaced in the node detail so a
    /// `failed`/`blocked` node explains itself.
    pub error: String,
}

/// A blackboard artifact projected for the blackboard view (Phase 5 STEP 5.3).
/// Self-contained — the TUI never depends on `codypendent-workflow`; the CLI maps
/// a `BlackboardItem` (its opaque JSON payload/author/evidence rendered to human
/// strings) into this shape. Items are grouped by their `run` label, so several
/// workflow runs' boards read as labeled groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlackboardItemCard {
    /// The owning workflow run, pre-rendered (e.g. `"repair-github-check · run
    /// 0f2a"`), so several runs' boards share the list under labeled groups.
    pub run: String,
    /// The artifact kind, pre-rendered (`finding` / `decision` / `proposed_patch`
    /// / …).
    pub kind: String,
    /// A one-line human summary of the artifact's payload.
    pub summary: String,
    /// Who produced it, pre-rendered from the author record (e.g. `"agent
    /// investigator"`).
    pub author: String,
    /// The producer's confidence, pre-rendered (`"0.85"` or `"—"`).
    pub confidence: String,
    /// The evidence backing the artifact, pre-rendered (e.g. `"2 ref(s)"` or
    /// `"—"`) — claim-like kinds always carry it.
    pub evidence: String,
    /// The artifact's revision, pre-rendered (e.g. `"r1"`).
    pub revision: String,
    /// Whether this item has been superseded by a later revision (the review
    /// rail shows the live item; a superseded one is dimmed).
    pub superseded: bool,
}

/// Where a model-picker card's model runs (MP1). A tui-local mirror of just
/// the two labels `codypendent_routing::ModelLocation` carries — the `tui`
/// crate speaks only `codypendent-protocol` and must never depend on
/// `codypendent-routing` (STEP 1.12 RULE 1), so this is a self-contained copy
/// the CLI harness maps a measured model profile's location onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLocationLabel {
    /// On-device (embedded, subprocess, or LAN service treated as local).
    Local,
    /// Off-device (a hosted/cloud provider).
    Hosted,
}

/// A model-picker card (MP1): one selectable model from `models.toml`,
/// enriched with its measured profile from the `model_profiles` table when one
/// exists. Self-contained — the TUI never depends on `codypendent-routing`;
/// the CLI harness maps a `ModelConfig` (plus any matching measured profile)
/// into this shape, exactly as it maps a `RegistryItem` into a [`SkillCard`].
/// "current" is not a field here: the [`Overlay::ModelPicker`] browser
/// computes it at render by comparing `id` to the active run's serving model
/// (`RunView::model`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCard {
    /// The model's id, as configured in `models.toml`.
    pub id: ModelId,
    /// The wire-protocol adapter this model uses (e.g. `"openai-compatible"`
    /// — the only value Phase 1 supports; see `ModelConfig::provider`).
    pub provider: String,
    /// Where the model runs, when a measured profile exists. `None` when the
    /// model has no `model_profiles` row (badges are best-effort;
    /// `models.toml` is the authoritative selectable list — STEP 1.9).
    pub location: Option<ModelLocationLabel>,
    /// The measured blended cost per 1K tokens, in USD, when a profile
    /// exists.
    pub cost_per_1k_usd: Option<f64>,
    /// The model's declared context window, in tokens, when a profile
    /// exists.
    pub context_tokens: Option<u64>,
}

/// The indices into `models` whose id or provider case-insensitively contains
/// `query` — the model picker's substring filter, in list order (mirrors
/// [`crate::palette::filtered`]'s shape, adapted to instance data rather than
/// a static table). An empty query matches every model. A free function
/// (rather than an `AppState` method) so a caller already holding a live
/// borrow of `AppState::overlay` can pass `&state.models` directly alongside
/// it without a borrow conflict.
#[must_use]
pub(crate) fn filter_models(models: &[ModelCard], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    models
        .iter()
        .enumerate()
        .filter(|(_, card)| {
            needle.is_empty()
                || card.id.0.to_lowercase().contains(&needle)
                || card.provider.to_lowercase().contains(&needle)
        })
        .map(|(idx, _)| idx)
        .collect()
}

/// Ceiling on retained transcript entries per run (the ledger is the durable
/// record; this is a bounded view for an in-terminal scrollback).
pub(crate) const MAX_TRANSCRIPT_ENTRIES: usize = 2000;
/// Ceiling on one coalesced model-text entry's bytes.
pub(crate) const MAX_MODEL_ENTRY_BYTES: usize = 256 * 1024;

/// The status-line projection (STEP 1.12 RULE 4, [Chapter 20] projections):
/// mode, run state, model, context %, cost, worktree, pending-approval count.
///
/// [Chapter 20]: ../../../docs/docs/20-interaction-and-autonomy-model.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusProjection {
    pub mode: Option<AgentMode>,
    pub run_state: Option<RunState>,
    pub model: Option<ModelId>,
    pub context_percent: Option<u16>,
    pub cost_minor: Option<u64>,
    pub worktree: Option<String>,
    pub pending_approvals: usize,
}

/// The whole application state. Read by the renderer, mutated only by `reduce`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppState {
    /// The attached session's title, once known.
    pub session_title: Option<String>,
    /// Whether the session has been closed.
    pub session_closed: bool,
    /// All runs, in arrival order.
    pub runs: Vec<RunView>,
    /// Index into `runs` of the selected run.
    pub selected_run: usize,
    /// Pending approvals across the session.
    pub pending_approvals: Vec<PendingApproval>,
    /// Index into `pending_approvals` of the focused approval.
    pub selected_approval: usize,
    /// The Skill Studio projection (STEP 2.6): every registered item, mapped to a
    /// self-contained [`SkillCard`] by the CLI. Populated once at attach; the
    /// [`Overlay::Skills`] browser reads it.
    pub skills: Vec<SkillCard>,
    /// Index into `skills` of the focused skill.
    pub selected_skill: usize,
    /// The memory projection (STEP 2.6): the visible-scope memories, mapped to
    /// self-contained [`MemoryCard`]s by the CLI. May be empty. The
    /// [`Overlay::Memory`] browser reads it.
    pub memories: Vec<MemoryCard>,
    /// Index into `memories` of the focused memory.
    pub selected_memory: usize,
    /// The Docs Studio projection (Phase 4 client wiring): the visible-scope
    /// documents, mapped to self-contained [`DocCard`]s by the CLI. May be
    /// empty. The [`Overlay::Docs`] browser reads it.
    pub docs: Vec<DocCard>,
    /// Index into `docs` of the focused document.
    pub selected_doc: usize,
    /// Index into the focused document's `blocks` of the focused block (the editor
    /// rail cursor; the edit action targets this block).
    pub selected_block: usize,
    /// Index into the focused document's `suggestions` of the focused suggestion
    /// (the review rail cursor; accept/reject target this suggestion).
    pub selected_suggestion: usize,
    /// Which rail of the Docs overlay the keyboard drives (`Tab` cycles it).
    pub doc_focus: DocFocus,
    /// The in-flight document edit (lease lifecycle + queued mutation), if any.
    /// Drives the editor rail's lease indicator and the lease→mutate handshake.
    pub doc_edit: Option<DocEdit>,
    /// The code-graph edge projection (Phase 4 exit criterion 4): this
    /// repository's edges, mapped to self-contained [`GraphEdgeCard`]s by the
    /// CLI. May be empty. The [`Overlay::Edges`] inspector reads it.
    pub edges: Vec<GraphEdgeCard>,
    /// Index into `edges` of the focused edge.
    pub selected_edge: usize,
    /// The workflow-graph projection (Phase 5 STEP 5.2): the nodes of the
    /// repository's compiled workflow manifests, mapped to self-contained
    /// [`WorkflowNodeCard`]s by the CLI, in topological order. May be empty. The
    /// [`Overlay::Workflow`] view reads it.
    pub workflow: Vec<WorkflowNodeCard>,
    /// Index into `workflow` of the focused node.
    pub selected_node: usize,
    /// The blackboard projection (Phase 5 STEP 5.3): the artifacts on the active
    /// workflow runs' boards, mapped to self-contained [`BlackboardItemCard`]s by
    /// the CLI, grouped by run. May be empty (until the executor posts artifacts).
    /// The [`Overlay::Blackboard`] view reads it.
    pub blackboard: Vec<BlackboardItemCard>,
    /// Index into `blackboard` of the focused item.
    pub selected_item: usize,
    /// The model-picker projection (MP1): every model configured in
    /// `models.toml`, enriched with its measured profile from
    /// `model_profiles` when one exists, mapped to a self-contained
    /// [`ModelCard`] by the CLI harness. Populated once at attach; the
    /// [`Overlay::ModelPicker`] browser reads it.
    pub models: Vec<ModelCard>,
    /// Index into `models` of the focused card — kept resolved to the
    /// picker's live filtered selection by the reducer, so
    /// [`AppState::focused_model`] reads uniformly with every other
    /// browser's `focused_*` accessor.
    pub selected_model: usize,
    /// The model staged from the picker (`Enter` on a row). Advisory only
    /// this task (MP1) — nothing yet reads it to change routing behavior; a
    /// later task (MP2) wires it to pin the next run's model.
    pub pending_model: Option<ModelId>,
    /// The focused pane. Vestigial in the conversation-centred shell (the
    /// transcript is the single main surface); retained for catch-up/mouse code.
    pub focus: Pane,
    /// The persistent composer buffer (the always-present bottom input). Typed
    /// text lands here; Enter sends it (starting a run, or steering the active
    /// one). Empty by default.
    pub composer: String,
    /// Which base layout is rendered (chat single-column vs. workspace panes).
    /// Toggled with `F2`; defaults to [`LayoutMode::Chat`].
    pub layout: LayoutMode,
    /// The maximum transcript scroll offset (rows below the top that still fill
    /// the viewport), cached by the renderer each frame. The renderer knows the
    /// wrapped height and viewport; the reducer reads this cache so PgUp can leave
    /// follow mode at the true bottom and PgDn can re-enter it. A one-frame-stale
    /// layout metric — never domain state — which is why it is a [`Cell`] the
    /// draw-only renderer may update through a shared reference.
    pub transcript_max_scroll: Cell<u16>,
    /// The top-most overlay / modal.
    pub overlay: Overlay,
    /// The mode used for the next new run (the new-run prompt inherits it).
    pub default_mode: AgentMode,
    /// Set when the user detaches (`q`). The CLI observes this to leave the TUI
    /// loop; the run is never affected.
    pub should_detach: bool,
    /// A monotonic tick counter for spinner animation.
    pub tick: u64,
    /// A transient status-line notice and the tick at which it expires.
    pub notice: Option<(String, u64)>,
    /// Semantic commands the CLI must send to the daemon. Drained by the CLI
    /// after every reduce; never touched by the renderer.
    pub outbox: Vec<Intent>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    /// A fresh, empty state (nothing attached yet).
    #[must_use]
    pub fn new() -> Self {
        Self {
            session_title: None,
            session_closed: false,
            runs: Vec::new(),
            selected_run: 0,
            pending_approvals: Vec::new(),
            selected_approval: 0,
            skills: Vec::new(),
            selected_skill: 0,
            memories: Vec::new(),
            selected_memory: 0,
            docs: Vec::new(),
            selected_doc: 0,
            selected_block: 0,
            selected_suggestion: 0,
            doc_focus: DocFocus::default(),
            doc_edit: None,
            edges: Vec::new(),
            selected_edge: 0,
            workflow: Vec::new(),
            selected_node: 0,
            blackboard: Vec::new(),
            selected_item: 0,
            models: Vec::new(),
            selected_model: 0,
            pending_model: None,
            focus: Pane::Sessions,
            composer: String::new(),
            layout: LayoutMode::Chat,
            transcript_max_scroll: Cell::new(0),
            overlay: Overlay::None,
            default_mode: AgentMode::Build,
            should_detach: false,
            tick: 0,
            notice: None,
            outbox: Vec::new(),
        }
    }

    /// The input mode the next key should be interpreted in.
    #[must_use]
    pub fn input_mode(&self) -> InputMode {
        match self.overlay {
            Overlay::NewRun(_) | Overlay::Steering(_) | Overlay::DocEdit { .. } => {
                InputMode::Editing
            }
            Overlay::ConfirmCancel => InputMode::Confirm,
            // The palette and the model picker both filter on printable keys
            // while staying arrow-navigable, so they share this input mode
            // (see [`crate::input::map_palette_key`]).
            Overlay::Palette { .. } | Overlay::ModelPicker { .. } => InputMode::Palette,
            // The Skills / Memory / Docs / Edges / Workflow / Help browsers are
            // navigable with the arrow/command key table, so they stay in `Normal`
            // mode.
            Overlay::Help
            | Overlay::Skills
            | Overlay::Memory { .. }
            | Overlay::Docs
            | Overlay::Edges
            | Overlay::Workflow
            | Overlay::Blackboard => InputMode::Normal,
            // The base conversation view: an unresolved approval owns the screen
            // (decision keys only); otherwise the composer captures typed text.
            Overlay::None => {
                if self.show_approval_modal() {
                    InputMode::Approval
                } else {
                    InputMode::Composer
                }
            }
        }
    }

    /// The currently selected run, if any.
    #[must_use]
    pub fn selected_run(&self) -> Option<&RunView> {
        self.runs.get(self.selected_run)
    }

    /// Whether the selected run is still live — i.e. a composer message should
    /// *steer* it rather than start a fresh run. `false` when no run is selected
    /// or the selected run has reached a terminal state.
    #[must_use]
    pub fn selected_run_is_active(&self) -> bool {
        self.selected_run().is_some_and(|run| {
            !matches!(
                run.state,
                RunState::Completed | RunState::Failed | RunState::Cancelled
            )
        })
    }

    /// Whether the approval modal should be shown: there is at least one pending
    /// approval and no other overlay is competing for the foreground.
    #[must_use]
    pub fn show_approval_modal(&self) -> bool {
        !self.pending_approvals.is_empty() && matches!(self.overlay, Overlay::None)
    }

    /// The focused pending approval, if any.
    #[must_use]
    pub fn focused_approval(&self) -> Option<&PendingApproval> {
        self.pending_approvals.get(self.selected_approval)
    }

    /// The focused Skill Studio card, if any.
    #[must_use]
    pub fn focused_skill(&self) -> Option<&SkillCard> {
        self.skills.get(self.selected_skill)
    }

    /// The focused memory card, if any.
    #[must_use]
    pub fn focused_memory(&self) -> Option<&MemoryCard> {
        self.memories.get(self.selected_memory)
    }

    /// The focused Docs Studio card, if any.
    #[must_use]
    pub fn focused_doc(&self) -> Option<&DocCard> {
        self.docs.get(self.selected_doc)
    }

    /// The focused block of the focused document, if any (the editor rail cursor).
    #[must_use]
    pub fn focused_block(&self) -> Option<&DocBlockView> {
        self.focused_doc()?.blocks.get(self.selected_block)
    }

    /// The focused suggestion of the focused document, if any (the review rail
    /// cursor).
    #[must_use]
    pub fn focused_suggestion(&self) -> Option<&DocSuggestionView> {
        self.focused_doc()?
            .suggestions
            .get(self.selected_suggestion)
    }

    /// The focused code-graph edge card, if any.
    #[must_use]
    pub fn focused_edge(&self) -> Option<&GraphEdgeCard> {
        self.edges.get(self.selected_edge)
    }

    /// The focused workflow node card, if any.
    #[must_use]
    pub fn focused_node(&self) -> Option<&WorkflowNodeCard> {
        self.workflow.get(self.selected_node)
    }

    /// The focused blackboard item card, if any.
    #[must_use]
    pub fn focused_item(&self) -> Option<&BlackboardItemCard> {
        self.blackboard.get(self.selected_item)
    }

    /// The focused model-picker card, if any.
    #[must_use]
    pub fn focused_model(&self) -> Option<&ModelCard> {
        self.models.get(self.selected_model)
    }

    /// Project the status-line fields from the selected run + pending approvals.
    #[must_use]
    pub fn status(&self) -> StatusProjection {
        let run = self.selected_run();
        StatusProjection {
            mode: run.map(|r| r.mode),
            run_state: run.map(|r| r.state),
            model: run.and_then(|r| r.model.clone()),
            context_percent: run.and_then(|r| r.context_percent),
            cost_minor: run.and_then(|r| r.cost_minor),
            worktree: run.and_then(|r| r.worktree.clone()),
            pending_approvals: self.pending_approvals.len(),
        }
    }

    /// Drain the outbox of intents accumulated since the last call. The CLI's
    /// connection task calls this after each reduce to dispatch commands.
    pub fn drain_outbox(&mut self) -> Vec<Intent> {
        std::mem::take(&mut self.outbox)
    }

    // --- internal helpers used by the reducer ---

    pub(crate) fn run_mut(&mut self, run_id: RunId) -> Option<&mut RunView> {
        self.runs.iter_mut().find(|r| r.run_id == run_id)
    }

    pub(crate) fn ensure_run(
        &mut self,
        run_id: RunId,
        objective: String,
        mode: AgentMode,
    ) -> &mut RunView {
        if let Some(idx) = self.runs.iter().position(|r| r.run_id == run_id) {
            // An already-known run re-announcing itself (catch-up overlap,
            // another client's activity) must not steal the selection.
            return &mut self.runs[idx];
        }
        self.runs.push(RunView::new(run_id, objective, mode));
        let last = self.runs.len() - 1;
        // Focus the new run unless the user is mid-draft. Our own submit
        // clears the composer before its RunStarted folds back, so this still
        // follows the action for runs this client starts — while another
        // client's RunStarted in a shared session cannot retarget a message
        // being composed (Enter submits against `selected_run` at that
        // moment).
        if self.composer.is_empty() {
            self.selected_run = last;
        }
        &mut self.runs[last]
    }

    pub(crate) fn selected_run_mut(&mut self) -> Option<&mut RunView> {
        self.runs.get_mut(self.selected_run)
    }

    /// Append model text, coalescing into a trailing `Model` entry.
    pub(crate) fn append_model_text(run: &mut RunView, text: &str) {
        if let Some(TranscriptEntry::Model { text: existing }) = run.transcript.last_mut() {
            // Bound one coalesced model entry: an hours-long stream must not grow
            // a single String without limit (the full text is in the ledger; the
            // transcript is a view). Past the cap, start a fresh entry so the
            // entry-count cap in `push_entry` takes over.
            if existing.len() + text.len() <= MAX_MODEL_ENTRY_BYTES {
                existing.push_str(text);
                return;
            }
        }
        Self::push_entry(
            run,
            TranscriptEntry::Model {
                text: text.to_owned(),
            },
        );
    }

    /// Append a transcript entry, holding the transcript to
    /// [`MAX_TRANSCRIPT_ENTRIES`] by dropping the oldest — the ledger, not this
    /// view, is the durable record. Selection/scroll indices shift with the
    /// drop so the focused entry stays the same one.
    pub(crate) fn push_entry(run: &mut RunView, entry: TranscriptEntry) {
        run.transcript.push(entry);
        while run.transcript.len() > MAX_TRANSCRIPT_ENTRIES {
            run.transcript.remove(0);
            run.transcript_selected = run.transcript_selected.saturating_sub(1);
            run.scroll = run.scroll.saturating_sub(1);
        }
    }
}
