//! Application state and its projections (STEP 1.12 RULE 2).
//!
//! [`AppState`] is the single source of truth the renderer reads. It is mutated
//! only by [`crate::reduce::reduce`]; it holds no I/O handles. All state is
//! derived deterministically from the ordered [`SessionEvent`] stream plus local
//! navigation, so replaying the same events yields the same state.

use codypendent_protocol::{
    AgentMode, ApprovalId, ArtifactRef, BudgetDimension, ChangeSetId, ModelId, ProposedAction,
    Risk, RunDisposition, RunId, RunState, ToolOutcome,
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

/// How the input layer should interpret the next key (see
/// [`crate::input::map_event`]). Derived from the active overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// A navigable overlay (Skills / Memory / Docs / Edges / Help) is live: the
    /// arrow/command key table drives it.
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
    /// The command palette: a searchable list of every command the TUI exposes,
    /// so the growing feature set stays reachable without consuming a single-key
    /// binding each. `query` is the live filter; `selected` indexes the filtered
    /// results (reset to 0 whenever the query changes). Opened with `/`.
    Palette { query: String, selected: usize },
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
    /// A note appended to the session.
    Note { text: String },
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

/// Everything known about one run, and its transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct RunView {
    pub run_id: RunId,
    pub objective: String,
    pub mode: AgentMode,
    pub state: RunState,
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
    /// Transcript scroll offset in rows.
    pub scroll: u16,
}

impl RunView {
    fn new(run_id: RunId, objective: String, mode: AgentMode) -> Self {
        Self {
            run_id,
            objective,
            mode,
            state: RunState::Queued,
            model: None,
            worktree: None,
            context_percent: None,
            cost_minor: None,
            disposition: None,
            transcript: Vec::new(),
            transcript_selected: 0,
            scroll: 0,
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
    /// The code-graph edge projection (Phase 4 exit criterion 4): this
    /// repository's edges, mapped to self-contained [`GraphEdgeCard`]s by the
    /// CLI. May be empty. The [`Overlay::Edges`] inspector reads it.
    pub edges: Vec<GraphEdgeCard>,
    /// Index into `edges` of the focused edge.
    pub selected_edge: usize,
    /// The focused pane. Vestigial in the conversation-centred shell (the
    /// transcript is the single main surface); retained for catch-up/mouse code.
    pub focus: Pane,
    /// The persistent composer buffer (the always-present bottom input). Typed
    /// text lands here; Enter sends it (starting a run, or steering the active
    /// one). Empty by default.
    pub composer: String,
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
            edges: Vec::new(),
            selected_edge: 0,
            focus: Pane::Sessions,
            composer: String::new(),
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
            Overlay::NewRun(_) | Overlay::Steering(_) => InputMode::Editing,
            Overlay::ConfirmCancel => InputMode::Confirm,
            // The palette filters on printable keys but stays arrow-navigable, so
            // it has its own input mode (see [`crate::input::map_palette_key`]).
            Overlay::Palette { .. } => InputMode::Palette,
            // The Skills / Memory / Docs / Edges / Help browsers are navigable with
            // the arrow/command key table, so they stay in `Normal` mode.
            Overlay::Help
            | Overlay::Skills
            | Overlay::Memory { .. }
            | Overlay::Docs
            | Overlay::Edges => InputMode::Normal,
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

    /// The focused code-graph edge card, if any.
    #[must_use]
    pub fn focused_edge(&self) -> Option<&GraphEdgeCard> {
        self.edges.get(self.selected_edge)
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
            self.selected_run = idx;
            return &mut self.runs[idx];
        }
        self.runs.push(RunView::new(run_id, objective, mode));
        self.selected_run = self.runs.len() - 1;
        let last = self.runs.len() - 1;
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
