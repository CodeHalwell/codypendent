//! Semantic actions (STEP 1.12 RULE 1).
//!
//! Everything that can change the app funnels through [`Action`]. Two sources
//! feed it: the CLI's connection task, which wraps each daemon [`SessionEvent`]
//! as [`Action::DaemonEvent`]; and the input layer ([`crate::input::map_event`]),
//! which turns a key/mouse/paste/resize into a navigation or command action.
//! The reducer ([`crate::reduce::reduce`]) is the only place that reads an
//! `Action`, and it performs no I/O.

use codypendent_protocol::{
    ApprovalId, ApprovalScope, DocumentId, DocumentMutation, PendingApprovalProjection, QuestionId,
    QuestionOutcome, RunId, SessionEvent, UiActionBinding, UiDocumentId, UiNodeId, UiRevision,
    UiWireMessage,
};

use crate::remote_ui::RemoteKey;
use crate::state::{
    AddModelRow, BlackboardItemCard, CouncilProgressPhase, CouncilRunSummary, DocBlockView,
    DocSuggestionView, KanbanCard, KeyStatus, ModelListOrigin, Pane, UnslothQuantCard,
    UnslothRepoCard, VoiceKeyRow,
};

/// One live workflow-node projection carried from the socket-owning harness to
/// the pure reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowNodeUpdate {
    pub node_id: String,
    pub state: String,
    pub cost: String,
    pub error: String,
}

/// A semantic action the reducer folds into [`crate::state::AppState`].
///
/// The large [`SessionEvent`] is boxed so every other (small) variant does not
/// pay for it — and so the whole enum stays cheap to move.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    // --- from the connection task ---
    /// A durable daemon event to fold into state.
    DaemonEvent(Box<SessionEvent>),
    /// A catch-up *snapshot* (the session was too far behind for an event
    /// replay): seed the session title, closed flag, and its active runs as
    /// stubs so a reopened long-running session is not blank until the next live
    /// event fills a run in.
    CatchupSnapshot {
        title: String,
        closed: bool,
        runs: Vec<RunId>,
        pending_approvals: Vec<PendingApprovalProjection>,
        pending_prompts: Vec<codypendent_protocol::PendingPromptView>,
    },
    /// Terminal focus gained (`true`) or lost (`false`) (Adoption 11 S4).
    TerminalFocus(bool),
    /// Pin or unpin the focused Session Library row (`Alt-P`).
    SessionLibraryTogglePin,
    /// Archive or restore the focused Session Library row (`Alt-A`).
    SessionLibraryToggleArchive,
    /// Open the rename prompt for the focused Session Library row (`Alt-R`).
    SessionLibraryBeginRename,
    /// Export the focused Session Library row (`Alt-E`).
    SessionLibraryExport,
    /// Session list loaded from daemon (Adoption 11 S1).
    SessionListLoaded(Vec<crate::state::SessionRow>),
    /// One ranked Session Library page came back. `query` is the query the page
    /// answers — a page for a query the operator has already typed past is
    /// discarded rather than shown under the wrong heading. `append` is true
    /// for a continuation page and false for a fresh first page.
    SessionSearchLoaded {
        query: String,
        rows: Vec<crate::state::SessionRow>,
        next_cursor: Option<codypendent_protocol::PageCursor>,
        append: bool,
    },
    /// A ranked Session Library search was REFUSED. Folded so the surface stops
    /// saying "searching…" for a page that is never coming — a refusal is an
    /// outcome, not an absence of one.
    SessionSearchFailed {
        query: String,
        reason: String,
    },
    /// The daemon's authoritative projection of a session after a lifecycle
    /// mutation. Folded in place so the library shows the state the daemon
    /// applied, never the state the client assumed.
    SessionLifecycleApplied(Box<crate::state::SessionRow>),
    /// A session was deleted (tombstoned or purged). It leaves the library.
    SessionLifecycleDeleted {
        session_id: codypendent_protocol::SessionId,
        tombstoned: bool,
    },
    /// A lifecycle export produced an artifact. The harness has already written
    /// the bytes; `path` is where they landed.
    SessionExported {
        session_id: codypendent_protocol::SessionId,
        path: String,
    },
    /// File search results loaded from daemon (Adoption 11 M2).
    FileSearchResults {
        query: String,
        matches: Vec<codypendent_protocol::command::FileMatchWire>,
        truncated: bool,
    },
    /// Move mention popup selection up / prev.
    MentionSelectPrev,
    /// Move mention popup selection down / next.
    MentionSelectNext,
    /// Confirm selected mention and insert into composer.
    MentionSelect,
    /// Dismiss mention popup.
    MentionCancel,
    /// Mouse path: complete the mention popup with the clicked match (the
    /// keyboard equivalent is `Up`/`Down` + `Enter` — RULE: keyboard parity).
    MentionSelectAt(usize),
    /// Walk history search selection to older matches.
    HistorySearchPrev,
    /// Walk history search selection to newer matches.
    HistorySearchNext,
    /// Accept selected history entry and load into composer.
    HistorySearchSelect,
    /// Mouse path: accept the clicked history-search row (the keyboard
    /// equivalent is `Up`/`Down` + `Enter` — RULE: keyboard parity). Carries
    /// the row's index in the popup's rendered match order.
    HistorySearchSelectAt(usize),
    /// Dismiss history search.
    HistorySearchCancel,
    /// Open the find-in-transcript popup (`Ctrl-F`). Client-only.
    TranscriptSearchOpen,
    /// Mouse path: jump to the clicked transcript-search match (keyboard
    /// equivalent: `Up`/`Down` + `Enter` — RULE: keyboard parity).
    TranscriptSearchSelectAt(usize),
    /// A periodic timer tick (spinner animation, elapsed timers). No I/O.
    Tick,
    /// A transient status-line notice from the harness (e.g. a rejected
    /// command's code + message). Cleared automatically a few seconds later.
    Notice(String),
    /// Open (or explicitly reopen) first-run setup at its triage screen. Used by
    /// both the post-splash gate and the zero-runnable Chat CTA.
    OpenOnboard,
    /// The daemon socket dropped; the harness is reconnecting. Shown as a
    /// persistent status-row state, not a five-second notice.
    LinkLost,
    /// The reconnect succeeded (or a fresh connection is live).
    LinkRestored,
    /// The harness probed the catalog's local model servers (P12): which of
    /// Ollama, LM Studio, vLLM (and any user-defined local provider) answered
    /// a TCP connect on its base URL. Replaces the previous probe wholesale.
    LocalEndpointsProbed(Vec<crate::state::LocalEndpoint>),
    /// Updated local provider projection after an off-thread registry refresh.
    ProviderCatalogLoaded(Vec<crate::state::ProviderCard>),
    /// Replace the harness-authoritative set of model profiles that can start a
    /// run without another setup/auth step. `onboard_attempt` correlates the
    /// refresh caused by one onboarding `AddModel`; only a matching runnable id
    /// may complete and persist onboarding. Boot/background refreshes use None.
    RunnableModelsRefreshed {
        model_ids: Vec<codypendent_protocol::ModelId>,
        onboard_attempt: Option<codypendent_protocol::ModelId>,
    },
    /// Persistence/connection failed before the host could perform a runnable
    /// refresh. Correlated by model id so an old ACP/write failure cannot kick a
    /// newer onboarding attempt backwards.
    OnboardModelAddFailed {
        model_id: codypendent_protocol::ModelId,
        reason: String,
    },
    /// The daemon rejected the exact in-flight first-run command. The reducer
    /// clears its admission guard and restores the submitted draft instead of
    /// leaving the composer blank or silently reopening admission.
    RunStartRejected {
        reason: String,
    },
    /// Push-to-talk capture started (`true`) or stopped (`false`), reported by
    /// the CLI's voice host (voice v1, rubric 8). Purely presentational: it
    /// drives the status-line recording indicator so a hot microphone is always
    /// visible. The capture itself is the host's subprocess, never the TUI's.
    VoiceRecording(bool),
    /// A persistent setup/runtime diagnostic from the harness. De-duplicated by
    /// the reducer and available in the Issues overlay until explicitly cleared.
    Issue(String),
    /// One validated Remote UI frame delivered by the daemon.
    RemoteUiMessage(Box<UiWireMessage>),
    /// Enter or leave keyboard focus for the mounted Remote UI surface.
    RemoteUiSetActive(bool),
    /// Move host focus to the next mounted Remote UI document without invoking
    /// any extension action. `Shift-F6` uses this path so every mounted
    /// document remains keyboard-reachable.
    RemoteUiNextDocument,
    /// Focus one mounted Remote UI document without activating a component.
    /// Registered beneath component hit regions so clicking chrome or inert
    /// content moves focus, while clicking an actual control still invokes it.
    RemoteUiFocusDocument(UiDocumentId),
    /// A key interpreted by the focused semantic component.
    RemoteUiKey {
        key: RemoteKey,
        character: Option<char>,
    },
    /// Bracketed paste for a focused semantic form field.
    RemoteUiPaste(String),
    /// Activate one revision-bound renderer hit region.
    RemoteUiActivate {
        document_id: UiDocumentId,
        revision: UiRevision,
        target_id: UiNodeId,
        binding: Box<UiActionBinding>,
    },
    /// Advertise a changed terminal viewport.
    RemoteUiViewport {
        width: u16,
        height: u16,
    },
    /// Authoritative lifecycle rows returned by a host-owned plugin command.
    UiPluginsLoaded(Vec<codypendent_protocol::UiPluginLifecycleStatus>),
    /// The daemon created a document. The reducer refreshes the Docs projection
    /// so the new row lands in the tree instead of dropping this reply.
    DocumentCreated {
        document_id: DocumentId,
    },
    /// A document publish plan is durably parked for approval. Unlike a notice,
    /// this preserves every field the ordinary approval card needs, including
    /// the originating document id correlated by the CLI command host.
    DocumentPublishPrepared {
        approval_id: ApprovalId,
        document_id: DocumentId,
        target: String,
        changed_files: Vec<String>,
        git_action: String,
    },
    /// The CLI host successfully persisted the reviewed council draft.
    CouncilCreated {
        name: String,
        members: usize,
        rounds: u8,
    },
    /// Council persistence failed. The reducer keeps the reviewed draft open so
    /// the operator can go back, correct it, and retry without starting over.
    CouncilCreateFailed {
        name: String,
        error: String,
    },
    /// The CLI host removed a council definition. Its saved run reports remain.
    CouncilDeleted {
        name: String,
    },
    /// Council removal failed (e.g. the store could not be rewritten).
    CouncilDeleteFailed {
        name: String,
        error: String,
    },
    /// One pre-formatted progress line from an off-thread council run (a round
    /// starting, a member completing/failing, or the chair beginning
    /// synthesis), pre-formatted host-side since this dependency-free crate
    /// cannot name `crate::council::CouncilEvent` (a cli-crate type). Folded
    /// into the active run's transcript as a Note.
    CouncilProgress {
        name: String,
        result_id: String,
        phase: CouncilProgressPhase,
        occurred_at: String,
        message: String,
        /// Council members/chair currently executing as independent agent runs.
        /// Kept explicit so shared UI telemetry treats council workers and
        /// workflow workers as the same first-class subagent concept.
        active_subagents: usize,
    },
    /// An off-thread council run finished. `Ok` carries the chair synthesis,
    /// attributed participants, and measured-cost line, pre-formatted
    /// host-side; `Err` is a human-readable failure (already naming any
    /// partial report path the run managed to save).
    CouncilRunFinished {
        name: String,
        result: Result<Box<CouncilRunSummary>, String>,
    },
    /// Replace the dedicated result-browser projection from durable storage.
    CouncilResultsLoaded(Vec<CouncilRunSummary>),
    CouncilResultsFailed(String),

    // --- navigation (from keys / mouse) ---
    /// Move keyboard focus to the next pane (`Tab`).
    CyclePane,
    /// Move keyboard focus to a specific pane (mouse click).
    FocusPane(Pane),
    /// Activate row N of the active list surface (mouse click): the open overlay
    /// list, or — with no overlay — the transcript fold line at entry N of the
    /// selected run. Folds to the same effect the keyboard's selection + `Enter`
    /// produces. Client-only (no `Intent`, no wire).
    ActivateRow(usize),
    /// Toggle one transcript fold by its full address (mouse click on a card,
    /// diff, or note head). Carries the owning run because the conversation
    /// stacks every run and a click can land on a card from any earlier turn —
    /// [`Action::ActivateRow`] can only ever name an entry of the *selected*
    /// run, which left every older card inert. Equivalent to walking the fold
    /// cursor there with `Alt-↑`/`Alt-↓` and pressing `Alt-Enter` (RULE 3).
    /// Client-only (no `Intent`, no wire).
    ActivateFold {
        run: usize,
        entry: usize,
    },
    /// Select run N in the runs pane (mouse click). Client-only.
    SelectRun(usize),
    /// Focus document N in the Docs tree (mouse click). Client-only.
    SelectDocument(usize),
    /// Focus block N in the Docs editor rail (mouse click). Client-only.
    SelectDocumentBlock(usize),
    /// Focus suggestion N in the Docs review rail (mouse click). Client-only.
    SelectDocumentSuggestion(usize),
    /// Select the previous item / scroll up in the focused pane (`Up`/`k`/wheel-up).
    SelectPrev,
    /// Select the next item / scroll down in the focused pane (`Down`/`j`/wheel-down).
    SelectNext,
    /// Move backward by a visible chunk in a picker (`PageUp`).
    SelectPagePrev,
    /// Move forward by a visible chunk in a picker (`PageDown`).
    SelectPageNext,
    /// Jump to the first picker result (`Home`).
    SelectFirst,
    /// Jump to the last picker result (`End`).
    SelectLast,
    /// Scroll the transcript up a page (`PageUp`).
    ScrollPageUp,
    /// Scroll the transcript down a page (`PageDown`).
    ScrollPageDown,
    /// Scroll the transcript up a few lines (one wheel notch). The finer
    /// sibling of [`Action::ScrollPageUp`]; a wheel notch that jumped ten rows
    /// read as a page jump, not a scroll.
    ScrollLinesUp,
    /// Scroll the transcript down a few lines (one wheel notch).
    ScrollLinesDown,
    /// Open / expand the selected item (`Enter`).
    Expand,
    /// Move the transcript fold selection to the previous foldable entry of the
    /// selected run (`Alt-↑` in the base conversation). Entering this browse
    /// mode is what makes tool cards and patch diffs keyboard-reachable:
    /// `Alt-Enter` then expands the browsed fold. Client-only.
    BrowseFoldPrev,
    /// Move the transcript fold selection to the next foldable entry
    /// (`Alt-↓`). Client-only.
    BrowseFoldNext,
    /// Step through changed files or hunks in the browsed patch preview.
    NavigatePatch {
        forward: bool,
        files: bool,
    },
    /// Open the browsed patch's current file/hunk in the same session's IDE.
    OpenReviewedSource,
    /// Copy the exact safe projection of the browsed transcript card. The
    /// harness owns the clipboard; failures use the same redacted text shown
    /// on screen, never the raw provider payload.
    CopyFocusedCard,
    /// Retry the failed selected run with its original objective/model/mode.
    RetryFailedRun,
    /// Open the masked auth editor for the failed model when applicable.
    ReauthenticateFailedModel,
    /// Open the model picker from a failed card so routing can be switched.
    ChooseFailureModel,
    /// Disable/remove the failed configured profile through the ordinary
    /// guarded model-removal confirmation.
    DisableFailureModel,

    // --- run control ---
    /// Switch the conversation to the previous run (`Ctrl-↑`).
    PrevRun,
    /// Switch the conversation to the next run (`Ctrl-↓`).
    NextRun,
    /// Open the new-run prompt (`n`).
    NewRun,
    /// Pause the selected run, or resume it if already paused (`p`).
    Pause,
    /// Ask to cancel the selected run — opens a confirm modal (`c`).
    Cancel,
    /// Confirm a pending cancel (`y`/`Enter` in the confirm modal).
    ConfirmCancel,
    /// Open the steering-input prompt (`s`).
    Steer,

    // --- approvals ---
    /// Approve the focused pending approval with the given scope
    /// (`a` = once, `A` = for the run).
    Approve(ApprovalScope),
    /// Reject the focused pending approval (`r`).
    Reject,

    // --- questions ---
    /// Move the question card selection up or down.
    QuestionNavigate(isize),
    /// Jump directly to a question option by digit (1..=9).
    QuestionPickDigit(usize),
    /// Toggle selection of the highlighted option.
    QuestionToggleOption,
    /// Select option / advance to next question / submit answers.
    QuestionSelectOrConfirm,
    /// Append a character to custom answer text or reject feedback.
    QuestionInputChar(char),
    /// Delete previous character in custom answer text or reject feedback.
    QuestionInputBackspace,
    /// Open the reject-feedback input prompt.
    QuestionOpenReject,
    /// Cancel/close the reject-feedback input prompt.
    QuestionCancelReject,
    /// Submit rejection (with optional feedback).
    QuestionSubmitReject,

    // --- text entry (active only while a prompt overlay is open) ---
    /// Append a character to the open prompt.
    InputChar(char),
    /// Insert bracketed-paste text into the open prompt.
    InputPaste(String),
    /// Delete the character before the cursor of the open prompt.
    InputBackspace,
    /// Move the composer cursor one grapheme left / right (`←`/`→`).
    /// Client-only.
    CursorLeft,
    /// Move the composer cursor one grapheme right (`→`). Client-only.
    CursorRight,
    /// Move the composer cursor to the start of the word before it
    /// (`Ctrl-←` / `Alt-←`, readline's `Alt-B`). Client-only.
    CursorWordLeft,
    /// Move the composer cursor past the end of the word after it
    /// (`Ctrl-→` / `Alt-→`, readline's `Alt-F`). Client-only.
    CursorWordRight,
    /// Move the composer cursor to the start of its current line (`Home`,
    /// `Ctrl-A`).
    CursorLineStart,
    /// Move the composer cursor to the end of its current line (`End`,
    /// `Ctrl-E`).
    CursorLineEnd,
    /// Delete the word before the cursor (`Ctrl-W`) — in the composer, or in
    /// whichever prompt/filter buffer is capturing text. Client-only.
    DeleteWordBack,
    /// Delete from the start of the current line to the cursor (`Ctrl-U`) —
    /// in the composer, or in whichever prompt/filter buffer is capturing
    /// text. Client-only.
    DeleteToLineStart,
    /// Delete from the composer cursor to the end of its line (`Ctrl-K`,
    /// readline's kill-to-end). Client-only.
    DeleteToLineEnd,
    /// Delete the grapheme under the composer cursor (`Delete` with no queued
    /// prompt selected — forward delete). Client-only.
    DeleteForward,
    /// Swap the composer with the draft the last `Esc` cleared (`Ctrl-Z`), so
    /// clearing a long draft is reversible. Client-only.
    DraftRestore,
    /// Delete the selected pending prompt when the queue selection is focused (Adoption 06).
    DeleteSelectedPrompt,
    /// Insert a manual line break into the open prompt (`Alt+Enter`) without
    /// submitting — the composer/prompt buffer already renders embedded `\n`
    /// as separate lines, growing to fit.
    InputNewline,
    /// Insert a line break even while browsing a transcript fold (Shift-Enter).
    InsertNewline,
    /// Submit the open prompt (`Enter`).
    InputSubmit,
    /// Abandon the open prompt (`Esc`).
    InputCancel,
    /// Recall the previous composer submission (`Up` in the base view,
    /// shell-style). The first press stashes the in-progress draft so it is
    /// never lost; repeated presses walk toward older entries. Client-only
    /// (no `Intent`, no wire) — history lives only in this client's state.
    HistoryPrev,
    /// Walk composer history toward newer entries (`Down` in the base view);
    /// moving past the newest restores the stashed in-progress draft.
    /// Client-only.
    HistoryNext,

    // --- knowledge browsers (STEP 2.6) ---
    /// Toggle the Skill Studio browser (`S`).
    OpenSkills,
    /// Toggle the memory browser (`M`).
    OpenMemory,
    /// Open the curated learning journey/review surface.
    OpenJourney,
    /// Open the token context usage card (/context).
    OpenContext,
    /// Reveal the focused memory's source in full (`o`, or `Enter` in the memory
    /// browser). The TUI does no I/O, so this surfaces the source string rather
    /// than opening a file.
    OpenSource,

    // --- Docs Studio & code intelligence (Phase 4 client wiring) ---
    /// Toggle the Docs Studio browser (`D`): tree / editor rail / review rail.
    OpenDocs,
    /// Toggle the code-graph edge inspector (`G`).
    OpenEdges,
    /// One database-backed code-graph result page returned by the harness.
    EdgesLoaded {
        edges: Vec<crate::state::GraphEdgeCard>,
        total: usize,
        query: String,
        page: usize,
    },
    /// Toggle the workflow-graph view (`W`): nodes with state, action, agent,
    /// worktree, approval, retry, dependencies, and declared outputs.
    OpenWorkflow,
    /// Toggle the blackboard view (`B`): the typed artifacts agents share within
    /// a workflow run, with author, confidence, evidence, and payload summary.
    OpenBlackboard,
    /// Toggle the repository task board (rubric 10): backlog cards laid out in
    /// status columns, live over the board's blackboard channel.
    OpenKanban,
    /// Move the focused board card one column to the right (`todo` → `doing` →
    /// `review` → `done`). A no-op on the last column.
    MoveCardForward,
    /// Move the focused board card one column to the left. A no-op on the first.
    MoveCardBack,
    /// Open the host-owned Remote UI plugin lifecycle surface.
    OpenUiPlugins,
    /// Toggle the council browser (`C`, rubric 6 TUI wiring): list, run, and
    /// manage persisted councils.
    OpenCouncils,
    /// Begin removal confirmation for the focused council (`d`, council
    /// browser only).
    DeleteCouncil,
    /// Smoke-test the selected plugin in the daemon sandbox.
    SmokeTestUiPlugin,
    /// Enable the selected plugin for only the attached session.
    EnableUiPluginSession,
    /// Enable the selected plugin for the current user across sessions.
    EnableUiPluginUser,
    /// Begin revocation confirmation for the selected plugin.
    RevokeUiPlugin,

    // --- Docs Studio live editing (Phase 4 STEP 4.3 client wiring) ---
    /// Begin editing the focused block in the Docs editor rail (`e`): opens the
    /// block-edit prompt. Submitting it acquires the block lease and, on the grant,
    /// sends the mutation.
    EditDoc,
    /// Publish the focused document to a repository Markdown file (`P`). The
    /// daemon computes the plan and parks its ordinary human approval first.
    PublishDoc,
    /// Open the new-document prompt in the Docs Studio. Reached from `n` while
    /// the Docs Studio is open (see [`Action::NewRun`], which the reducer routes
    /// by overlay exactly as it routes `n` to "start the focused workflow" in
    /// the workflow browser), never bound to its own global key.
    NewDoc,
    /// Open the insert-block prompt for a new paragraph below the focused block
    /// (`i`, editor rail).
    InsertDocBlock,
    /// Ask to delete the focused block (`x`, editor rail); routes through a
    /// confirmation before any mutation is sent.
    DeleteDocBlock,
    /// A merged replica update, projected by the CLI harness after it folded an
    /// incoming `DocumentSync` into the document's client replica and re-read its
    /// pending suggestions. Replaces the matching card's blocks/suggestions/revision
    /// so the editor reflects the authoritative result. The whole CRDT merge stays
    /// in the harness (which owns the Loro replica); the reducer folds the ready
    /// projection.
    DocumentSynced {
        document_id: DocumentId,
        /// The document's revision after the sync, pre-rendered (e.g. `"r8"`).
        revision: String,
        blocks: Vec<DocBlockView>,
        suggestions: Vec<DocSuggestionView>,
    },
    /// The daemon granted the edit lease this client requested — the reply the
    /// harness forwards from `Payload::DocumentLeaseGranted`. Marks the in-flight
    /// edit *held* and releases its queued mutation.
    DocumentLeaseGranted {
        document_id: DocumentId,
        lease_id: String,
    },
    /// The daemon refused the edit lease: the block range is held by another writer
    /// (`document.range-leased`). Marks the in-flight edit *blocked* and surfaces a
    /// visible notice — the presence-lite "someone else is editing" signal.
    DocumentLeaseBlocked,

    /// A live workflow node transition, projected by the CLI harness after it folded
    /// an incoming `Payload::WorkflowEvent` (Phase 5 T9). Overlays the matching
    /// workflow-graph card's live `state` / `cost` / `error` (each pre-rendered by the
    /// harness), so the graph view's `node_state_color` branches come alive as the run
    /// advances. The whole subscription/rendering stays in the harness (which owns the
    /// socket); the reducer folds the ready projection by node id — idempotent
    /// overwrite, so an overlap between the snapshot baseline and the live stream is a
    /// harmless re-write.
    WorkflowNodeUpdated {
        /// The durable workflow run this transition belongs to.
        workflow_run_id: String,
        /// The node (step) id to overlay — matches [`WorkflowNodeCard::id`].
        node_id: String,
        /// The node's live state, pre-rendered (e.g. `running` / `completed` /
        /// `skipped`).
        state: String,
        /// The node's measured cost, pre-rendered (e.g. `"12s · 3 tool calls"`), or
        /// `"—"` when none.
        cost: String,
        /// The node's failure/block reason, pre-rendered, or `"—"` when none.
        error: String,
    },
    /// Full live baseline returned by `ReadWorkflowRun`.
    WorkflowSnapshotLoaded {
        workflow_run_id: String,
        phase: String,
        nodes: Vec<WorkflowNodeUpdate>,
    },
    /// A live workflow-run phase transition.
    WorkflowPhaseUpdated {
        workflow_run_id: String,
        phase: String,
    },
    /// Replace one run's blackboard baseline after `ReadBlackboard`.
    BlackboardLoaded {
        workflow_run_id: String,
        items: Vec<BlackboardItemCard>,
    },
    /// Merge one live `BlackboardPosted` delivery by stable artifact id.
    BlackboardItemUpdated(BlackboardItemCard),
    /// Replace the repository task board's baseline after a board-scoped
    /// `ReadBlackboard` (rubric 10).
    BoardLoaded(Vec<KanbanCard>),
    /// Merge one live board delivery by card id — an agent's `task.create`, or
    /// the replacement a move produced. A card carrying a supersession is dropped
    /// rather than merged: its replacement arrives as its own delivery, so the
    /// board never shows both revisions of one card.
    BoardCardUpdated {
        card: KanbanCard,
        superseded: bool,
    },

    /// A provider's model list, fetched (or cache-seeded) by the harness
    /// (client-only add-model flow). Folds into the in-flight
    /// `Overlay::AddModelQuerying` (matched by `provider_id`) →
    /// `Overlay::AddModelPick`, and folds again into an already-open
    /// `AddModelPick` for the same provider so a cached seed can be replaced
    /// by the live refresh under the operator's fingers without losing their
    /// filter. Carries NO key — the key stays in the reducer's overlay across
    /// the round trip.
    ProviderModelsLoaded {
        provider_id: String,
        models: Vec<AddModelRow>,
        origin: ModelListOrigin,
    },
    /// The model-list query failed (unreachable / non-200 / unparseable / auth
    /// rejected / empty) AND the catalog had no rows to fall back on. `reason`
    /// is a human, key-free message. Folds the in-flight query into the
    /// free-text `Overlay::AddModelId` fallback (carrying any already-entered
    /// key). A failure with catalog rows available arrives as
    /// `ProviderModelsLoaded` with a `Catalog` origin instead.
    ProviderModelsFailed {
        provider_id: String,
        reason: String,
    },
    /// The result of a one-shot `/keys` key verification (`Ctrl-T`): a single
    /// `/models` call against the model's endpoint with the stored key.
    /// `reason` is key-free. Upgrades that model's card readiness from
    /// `Unverified` to the honest answer.
    ModelKeyVerified {
        model_id: String,
        ok: Option<bool>,
        reason: String,
    },

    /// The Unsloth org's GGUF repo listing arrived (Hugging Face Hub
    /// discovery). Folds into the in-flight `Overlay::UnslothRepos { loading:
    /// true, .. }`, flipping `loading` false and filling `repos`.
    UnslothReposLoaded(Vec<UnslothRepoCard>),
    /// The repo listing failed (unreachable / non-200 / unparseable).
    /// `reason` is a human message. Closes the overlay with a notice — this
    /// flow has no free-text fallback to fall back to.
    UnslothReposFailed(String),
    /// The quant-variant listing for `repo_id` arrived. `repo_id` guards
    /// against a stale reply landing after the operator picked a different
    /// repo (mirrors `ProviderModelsLoaded`'s `provider_id` guard) — folds
    /// into `Overlay::UnslothQuants` only when it still names this repo.
    UnslothQuantsLoaded {
        repo_id: String,
        quants: Vec<UnslothQuantCard>,
    },
    /// The quant-variant listing failed. `repo_id` guards exactly like
    /// [`Action::UnslothQuantsLoaded`].
    UnslothQuantsFailed {
        repo_id: String,
        reason: String,
    },
    /// One parsed line of `ollama pull` output. Appended to the in-flight
    /// `Overlay::UnslothPulling` only when `repo_id`/`quant` still match (a
    /// late line after the operator dismissed the overlay is dropped).
    UnslothPullProgress {
        repo_id: String,
        quant: String,
        line: String,
    },
    /// The pull (and, on success, the `models.toml` registration) finished.
    /// `result` is `Ok(registered_id)` or `Err(human message)`. Guarded by
    /// `repo_id`/`quant` exactly like [`Action::UnslothPullProgress`].
    UnslothPullFinished {
        repo_id: String,
        quant: String,
        result: Result<String, String>,
    },

    /// The `/keys` status projection (D1), loaded by the harness after the
    /// other projections (it reads `auth.json` + `models.toml` — the tui crate
    /// does no I/O) and re-fired after every key write.
    /// `models` is one `(model_id, status)` per configured model; `tavily` is
    /// the `web.search` row's status; `voice` is one row per CONFIGURED
    /// `[transcription]`/`[speech]` table (empty when neither is). Statuses
    /// carry no key material — an env status holds the variable NAME, never its
    /// value.
    ApiKeyStatusesLoaded {
        models: Vec<(String, KeyStatus)>,
        tavily: KeyStatus,
        voice: Vec<VoiceKeyRow>,
    },
    /// Toggle the command palette (`/`): a searchable list of every command.
    OpenPalette,
    /// Begin the add-model flow for the focused provider in the `/provider`
    /// picker (`Tab`; `Enter` does the same). Branches on the provider's gates:
    /// a can-list provider queries its `/models` list; a cannot-list one opens
    /// the free-text name prompt. A no-op outside the provider picker.
    BeginAddModel,
    /// Begin removal of the focused persisted item (`Delete`): a configured
    /// model in `/model`, or a stored credential in `/keys`. The reducer owns
    /// the overlay-specific confirmation and makes this a no-op everywhere
    /// else.
    RemoveSelected,
    /// Verify the focused `/keys` row's key against its provider (`Ctrl-T`):
    /// one `/models` call, then `ModelKeyVerified`. A no-op outside that
    /// overlay, and on the Tavily row (it has no model endpoint to probe).
    VerifyApiKey,
    /// Re-fetch the open add-model pick-list from the provider, bypassing the
    /// on-disk cache (`Ctrl-R`). A no-op outside `Overlay::AddModelPick`.
    RefreshProviderModels,
    /// Flip between the chat single-column and the workspace panes (`F2`).
    ToggleLayout,

    // --- overlays / lifecycle ---
    /// Toggle the help overlay (`?`).
    Help,
    /// Toggle the persistent setup/diagnostics overlay.
    OpenIssues,
    /// Clear the diagnostics list and close its overlay.
    ClearIssues,
    /// Detach this client (`q`). Never kills the run.
    Detach,
    /// Dismiss the top-most overlay / modal (`Esc`).
    Dismiss,

    /// A fork request failed on the daemon side (Adoption 05).
    SessionForkFailed(codypendent_protocol::CodypendentError),

    /// A recognized-but-inert event (e.g. an unmapped key). Kept so the input
    /// mapper can stay total and callers never juggle `Option`.
    NoOp,
}

impl Action {
    /// Convenience constructor that boxes the event for [`Action::DaemonEvent`].
    #[must_use]
    pub fn daemon_event(event: SessionEvent) -> Self {
        Action::DaemonEvent(Box::new(event))
    }
}

/// A secret API key carried from the add-model flow to the CLI harness (the one
/// place that performs I/O), for a hosted provider. The `tui` crate never writes
/// it to disk; the harness stores it in `auth.json` (mode `0600`). `Debug` is
/// hand-written to REDACT the value — mirroring
/// `codypendent_providers::credential::ResolvedCredential` — so a stray
/// `{intent:?}` can never leak the key into a log or a snapshot. `PartialEq`/`Eq`
/// compare the inner value (so a test can assert on the exact key it supplied).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey(pub String);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

/// What an API key applies to in the `/keys` flow (D1). Carried by
/// [`Overlay::ApiKeySet`]/[`Overlay::ApiKeyRemoveConfirm`] and the
/// [`Intent::SetApiKey`]/[`Intent::RemoveApiKey`] intents. Never carries key
/// material — `Model` holds the (non-secret) model id, which doubles as the
/// `auth.json` entry key; the harness maps `Tavily` onto the reserved
/// `integrations/tavily` entry id.
///
/// [`Overlay::ApiKeySet`]: crate::state::Overlay::ApiKeySet
/// [`Overlay::ApiKeyRemoveConfirm`]: crate::state::Overlay::ApiKeyRemoveConfirm
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyTarget {
    /// A configured model's key (the `models.toml` id).
    Model(String),
    /// The Tavily `web.search` key.
    Tavily,
    /// The `[transcription]` (speech-to-text) endpoint's key. A voice profile
    /// is not a `[[model]]` entry — it has no [`ModelId`] and so can never
    /// appear as a `Model` row — which is why it needs its own target rather
    /// than reusing one. The harness maps it onto the `auth.json` entry the
    /// runtime's `audio_api_key` reads for that table.
    ///
    /// [`ModelId`]: codypendent_protocol::ModelId
    Transcription,
    /// The `[speech]` (text-to-speech) endpoint's key. See
    /// [`KeyTarget::Transcription`].
    Speech,
}

/// A disk-backed advanced-view projection the CLI can refresh without a daemon
/// command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionKind {
    Skills,
    Memory,
    Journey,
    Docs,
    Workflow,
}

/// A semantic command the reducer wants sent to the daemon.
///
/// The TUI performs no I/O, so instead of talking to the daemon it appends an
/// `Intent` to [`crate::state::AppState::outbox`]. The CLI's connection task
/// drains the outbox after each reduce and turns each intent into a protocol
/// `Command`. This keeps `reduce` pure and unit-testable: a test asserts on the
/// intents produced, never on a socket.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Client-only editor handoff; the harness supplies the attached session.
    OpenReviewedSource {
        run_id: RunId,
        file: String,
        line: u32,
    },
    /// Send a renderer-originated Remote UI frame on the attached session.
    RemoteUiMessage(Box<UiWireMessage>),
    /// Read daemon-owned installed-plugin lifecycle state.
    ListUiPlugins,
    SmokeTestUiPlugin {
        plugin_id: String,
    },
    EnableUiPlugin {
        plugin_id: String,
        scope: String,
    },
    ApproveUiPluginUpdate {
        plugin_id: String,
        receipt: String,
    },
    RejectUiPluginUpdate {
        plugin_id: String,
        receipt: String,
    },
    RevokeUiPlugin {
        plugin_id: String,
    },
    /// Start a new run in the attached session.
    StartRun {
        objective: String,
        mode: codypendent_protocol::AgentMode,
        /// The model the operator pinned via the `/model` picker (STEP MP2),
        /// carried so the started run executes on exactly that model. `None`
        /// lets the daemon resolve/route the model as before.
        model: Option<codypendent_protocol::ModelId>,
    },
    /// Continue the attached session with a follow-up message once the
    /// selected run has reached a terminal state (continuous-session plan,
    /// Task 5): the daemon reconstructs the session's prior turns from its
    /// event ledger and seeds a new continuation run, rather than starting a
    /// context-free one (an active run instead steers via
    /// [`Intent::QueueSteering`], unchanged). Mirrors [`Intent::StartRun`]'s
    /// `mode` field; the session id is supplied by the harness the same way
    /// `StartRun`'s is (see `intent_to_command`).
    ///
    /// Carries the current pinned `model` so a mid-conversation model switch is
    /// instant: when the operator re-picks a model in the `/model` picker, the
    /// very next follow-up in the SAME session runs on it, no restart and no new
    /// session. `Some(id)` pins this continuation (and becomes the session's
    /// current model server-side); `None` lets the daemon INHERIT the session's
    /// existing model (I-2) from its provenance, exactly as before. Repository
    /// (I-1) is still never carried here: it is stable across a session and the
    /// client is not authoritative for it.
    SubmitUserInput {
        text: String,
        mode: codypendent_protocol::AgentMode,
        /// The model the operator pinned via the `/model` picker, carried so a
        /// re-pick applies to this very follow-up (mid-conversation model
        /// switch). `None` inherits the session's current model server-side.
        model: Option<codypendent_protocol::ModelId>,
    },
    /// Resolve a pending approval.
    ResolveApproval {
        approval_id: codypendent_protocol::ApprovalId,
        decision: codypendent_protocol::ApprovalDecision,
        scope: ApprovalScope,
    },
    /// Resolve a pending question.
    ResolveQuestion {
        question_id: QuestionId,
        outcome: QuestionOutcome,
    },
    /// Pause a run.
    PauseRun {
        run_id: codypendent_protocol::RunId,
    },
    /// Resume a paused run.
    ResumeRun {
        run_id: codypendent_protocol::RunId,
    },
    /// Cancel a run.
    CancelRun {
        run_id: codypendent_protocol::RunId,
    },
    /// Queue steering text to apply at the next safe point.
    QueueSteering {
        run_id: codypendent_protocol::RunId,
        text: String,
    },
    /// Queue a prompt on the session's server-side pending queue (Adoption 06).
    QueuePrompt {
        text: String,
        mode: codypendent_protocol::AgentMode,
        delivery: codypendent_protocol::PromptDelivery,
    },
    /// Edit a queued prompt in place.
    UpdateQueuedPrompt {
        prompt_id: codypendent_protocol::PromptId,
        text: String,
    },
    /// Promote a queued prompt to steer (Enter on selected queue row).
    PromoteQueuedPrompt {
        prompt_id: codypendent_protocol::PromptId,
    },
    /// Remove a queued prompt without running it.
    DeleteQueuedPrompt {
        prompt_id: codypendent_protocol::PromptId,
    },

    // --- Docs Studio live editing (Phase 4 STEP 4.3 client wiring) ---
    /// Acquire (or renew) the edit lease over a document block before mutating it.
    /// The harness ensures it is subscribed to the document's sync stream first,
    /// then sends `AcquireDocumentLease`.
    AcquireDocumentLease {
        document_id: DocumentId,
        /// The block to lease (`None` = a whole-document structural lease).
        block_id: Option<String>,
    },
    /// Release a held document lease by its id.
    ReleaseDocumentLease {
        lease_id: String,
    },
    /// Apply a semantic mutation to a document (a direct edit, a proposed edit, or
    /// an accept/reject of a suggestion). The daemon's collaboration mode decides
    /// whether a content edit applies directly or lands as a suggestion.
    MutateDocument {
        document_id: DocumentId,
        mutation: DocumentMutation,
    },
    PublishDocument {
        document_id: DocumentId,
        target: codypendent_protocol::PublishTarget,
    },
    /// Create a collaborative document with `title` (rubric #4 doc-writer). The
    /// harness sends `CreateDocument` and refreshes the Docs projection so the
    /// new document appears in the tree.
    CreateDocument {
        title: String,
    },
    /// Subscribe to a document's live sync stream without mutating it. This is
    /// client-only: opening/focusing a document should keep the Docs Studio
    /// projection current even when this client is only reviewing it.
    WatchDocument {
        document_id: DocumentId,
    },
    /// Load one filtered code-graph page directly from SQLite. Client-only.
    SearchEdges {
        query: String,
        page: usize,
    },
    /// Reload one disk-backed advanced view. Client-only.
    RefreshProjection {
        kind: ProjectionKind,
    },
    /// Apply one local governed-learning mutation. The CLI validates the
    /// optimistic revision and refreshes the projection after success.
    MutateLearning {
        id: String,
        revision: u64,
        mutation: LearningMutation,
    },

    // --- durable workflow control + live observation ---
    /// Start the named workflow from the repository/user workflow registry.
    StartWorkflow {
        workflow_id: String,
        inputs: serde_json::Value,
    },
    /// Subscribe to and read a durable workflow run plus its blackboard. This is
    /// client-only; the harness grows the attach subscriptions and issues both
    /// read baselines before swallowing the intent.
    WatchWorkflow {
        workflow_run_id: String,
    },
    PauseWorkflow {
        workflow_run_id: String,
    },
    ResumeWorkflow {
        workflow_run_id: String,
    },
    RetryWorkflowNode {
        workflow_run_id: String,
        node_id: String,
    },
    CancelWorkflow {
        workflow_run_id: String,
    },

    // --- repository task board (rubric 10) ---
    /// Subscribe to and read the repository's task board. Client-only, exactly
    /// like [`WatchWorkflow`](Intent::WatchWorkflow): the harness grows the attach
    /// subscriptions to the board's channel and issues the `ReadBlackboard`
    /// baseline before swallowing the intent. The repository is supplied by the
    /// harness (the client is not authoritative for it), so this carries nothing.
    WatchBoard,
    /// Move a board card into another column. Applied as a supersession by the
    /// daemon, so the card's history survives the move.
    MoveBoardCard {
        item_id: String,
        status: String,
    },
    /// Create one operator-authored task in the repository Kanban board.
    CreateBoardCard {
        title: String,
    },
    /// Post an operator-authored open question to a workflow Blackboard.
    PostBlackboardQuestion {
        workflow_run_id: String,
        text: String,
    },

    /// Add a usable model from the TUI (client-only — NOT a daemon command). The
    /// harness maps this to local `models.toml` + `auth.json` writes and never
    /// sends an envelope, so it is intercepted in the drain loop before
    /// `intent_to_command`. `display_id` is the `models.toml` id (the flow
    /// defaults it to `<provider>/<model>`); `provider_id` selects the catalog
    /// entry the harness reads `base_url` from; `model` is the provider-side model
    /// name. `api_key` is the entered key for a hosted provider (redacted in
    /// `Debug`), or `None` for a local/no-auth provider. `context_tokens` is
    /// the picked row's known context window (catalog or `/models` metadata),
    /// persisted so the context gauge and the `num_ctx` hint work from the
    /// first run; `None` when nothing is known — never a guess.
    AddModel {
        display_id: String,
        provider_id: String,
        model: String,
        api_key: Option<SecretKey>,
        context_tokens: Option<u64>,
    },

    /// Remove one configured model from `models.toml` (client-only — NOT a
    /// daemon command). The harness performs a formatting-preserving atomic
    /// edit, removes any model-specific `auth.json` credential, and reloads
    /// the picker projection. Provider catalogue entries and ACP registry
    /// installations are deliberately outside this operation.
    RemoveModel {
        model_id: String,
    },

    /// Query a provider's OpenAI-compatible model list (client-only — NOT a
    /// daemon command). The harness seeds from `<data_dir>/model_lists/` and
    /// GETs `<base_url>/models` with the provider's auth header, feeding both
    /// back as `Action::ProviderModelsLoaded` / `ProviderModelsFailed`.
    /// `api_key` is the key the user entered for a hosted provider (redacted
    /// in `Debug`), or `None` — in which case the harness falls back to the
    /// provider-wide key already in `auth.json`. `refresh` skips the cache
    /// seed (the overlay's manual `Ctrl-R`). Intercepted in the harness drain
    /// loop, mirroring `AddModel`; never mapped to a `CommandBody`.
    QueryProviderModels {
        provider_id: String,
        api_key: Option<SecretKey>,
        refresh: bool,
    },
    /// Refresh the downloadable ACP catalogue without blocking input.
    RefreshAgentCatalog,

    /// Verify one model's stored key with a single `/models` call (client-only
    /// — the key never leaves the machine, exactly like [`Intent::SetApiKey`]).
    /// The harness answers with `Action::ModelKeyVerified`.
    VerifyApiKey {
        model_id: String,
    },

    /// Remember the theme picked in `/theme` so the next launch starts in it
    /// (client-only — a display preference, never a daemon command). The
    /// harness persists the id beside its session store; `theme_select` reads
    /// it at boot, below `--theme`/`CODYPENDENT_THEME`. The live switch does
    /// not depend on this: the renderer already draws in the picked theme.
    SetTheme {
        id: String,
    },
    /// Persist successful first-run setup. Emitted only after a correlated,
    /// authoritative runnable-model refresh confirms the submitted profile.
    /// Probe the local model servers again (P12). Client-only, like the
    /// `/keys` and model intents around it: nothing crosses the wire, the
    /// harness performs the TCP probe and answers with
    /// [`Action::LocalEndpointsProbed`]. It is an intent rather than a slot on
    /// the state so it travels the ONE effect channel every driver already
    /// drains (TUI invariant 7).
    ProbeLocalEndpoints,
    SetOnboardComplete,
    /// Persist the explicit "skip forever" choice from the onboarding shell.
    SetOnboardSkipped,
    /// Set (or replace) an API key from the `/keys` overlay (D1; client-only —
    /// NOT a daemon command, keeping the key off the wire exactly like
    /// `AddModel`). The harness writes it to `auth.json` (load-before-write,
    /// atomic, mode `0600`); model and Tavily credentials are resolved at use
    /// time, so no wire command or daemon restart exists. Intercepted in the
    /// harness drain loop; never mapped to a `CommandBody`.
    SetApiKey {
        target: KeyTarget,
        key: SecretKey,
    },
    /// Remove a saved API key from `auth.json` (D1; client-only, mirroring
    /// [`Intent::SetApiKey`]). Intercepted in the harness drain loop; never
    /// mapped to a `CommandBody`.
    RemoveApiKey {
        target: KeyTarget,
    },
    /// Persist a council assembled by the host-owned TUI wizard. Client-only:
    /// the CLI harness validates the configured model profiles and atomically
    /// writes private `councils.toml`; it is never a daemon command.
    CreateCouncil {
        name: String,
        description: String,
        members: Vec<(String, String)>,
        chair: String,
        rounds: u8,
    },
    /// Remove a persisted council definition (rubric 6 TUI wiring). Client-only,
    /// exactly like `CreateCouncil`: the harness calls
    /// `council::remove_definition` and reloads the browser projection. Saved
    /// run reports are left on disk.
    DeleteCouncil {
        name: String,
    },
    /// Run a persisted council's deliberation for `objective` (rubric 6 TUI
    /// wiring). Client-only: member/chair runs are independent daemon
    /// sessions, so the harness drives `council::run_with_progress` off-thread
    /// over its OWN connection, streaming progress and the final outcome back
    /// through the `ReaderSignal` channel — never a single `CommandBody`. No
    /// `repository` field: like `StartRun`, the harness fills it in from the
    /// attached session's own repository, not from reducer-owned state.
    RunCouncil {
        name: String,
        objective: String,
    },
    /// Read council outcomes only from the durable council store. `None` loads
    /// the newest result for every council; a selector is a council name or id.
    LoadCouncilResults {
        selector: Option<String>,
    },
    /// Ask the host terminal to put exact report text on the clipboard.
    CopyText {
        text: String,
    },
    /// Create and attach to a fresh session without leaving the TUI. Client-only:
    /// the harness creates the session, swaps this connection's attachment, and
    /// updates the repo→session continuity store while the old run continues.
    NewConversation,

    /// List the Unsloth org's GGUF repos from the Hugging Face Hub
    /// (client-only — NOT a daemon command, and never touches the network on
    /// this thread). The harness fetches off the UI thread and feeds the
    /// result back as `Action::UnslothReposLoaded` / `UnslothReposFailed`.
    ListUnslothRepos,
    /// List `repo_id`'s quant variants (client-only), mirroring
    /// [`Intent::ListUnslothRepos`]. Feeds back as
    /// `Action::UnslothQuantsLoaded` / `UnslothQuantsFailed`.
    ListUnslothQuants {
        repo_id: String,
    },
    /// Drive `ollama pull hf.co/<repo_id>:<quant>` and, on success, register
    /// the model in `models.toml` against the `ollama` provider
    /// (client-only). Feeds back zero or more `Action::UnslothPullProgress`
    /// as the pull streams output, then exactly one
    /// `Action::UnslothPullFinished`.
    PullUnslothModel {
        repo_id: String,
        quant: String,
    },
    /// Fork the current session at a checkpoint boundary (Adoption 05).
    ForkSession {
        checkpoint: codypendent_protocol::CheckpointId,
        prompt: String,
    },
    /// Emit a desktop notification while terminal is unfocused (Adoption 11 S4).
    Notify {
        message: String,
    },
    /// List sessions known to the daemon for the session picker (Adoption 11 S1).
    ListSessions,
    /// Ranked, cursor-paged Session Library search. `cursor` is `None` for the
    /// first page of a query and the daemon-issued continuation token for each
    /// page after it — the token is opaque to the TUI and is only ever echoed
    /// back.
    SearchSessions {
        query: String,
        cursor: Option<codypendent_protocol::PageCursor>,
    },
    /// Rename / pin / unpin / archive / restore / delete / export one session
    /// from the Session Library. The daemon remains the policy authority: a
    /// refusal comes back as an ordinary rejection notice.
    MutateSession {
        session_id: codypendent_protocol::SessionId,
        action: codypendent_protocol::SessionLifecycleAction,
    },
    /// Switch live attachment to an existing session (Adoption 11 S1).
    SwitchSession(codypendent_protocol::SessionId),
    /// Search workspace files fuzzy-matching query (Adoption 11 M2).
    SearchFiles {
        query: String,
    },
    /// Run an operator shell command via `!cmd` prefix (Action 18).
    RunUserShell {
        command: String,
    },
    /// Curate an operator-authored memory via `#text` prefix (Action 20).
    RememberMemory {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearningMutation {
    Activate,
    Reject,
    SetPinned(bool),
    EditStatement(String),
    Delete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_debug_is_redacted() {
        let k = SecretKey("sk-super-secret".to_string());
        let dbg = format!("{k:?}");
        assert!(
            !dbg.contains("sk-super-secret"),
            "key redacted in Debug: {dbg}"
        );
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn add_model_intent_debug_redacts_the_key() {
        let intent = Intent::AddModel {
            display_id: "groq/llama".to_string(),
            provider_id: "groq".to_string(),
            model: "llama-3.1-8b".to_string(),
            api_key: Some(SecretKey("sk-secret".to_string())),
            context_tokens: None,
        };
        assert!(
            !format!("{intent:?}").contains("sk-secret"),
            "the key must never leak through the intent's Debug"
        );
    }

    #[test]
    fn query_provider_models_intent_debug_redacts_the_key() {
        let intent = Intent::QueryProviderModels {
            provider_id: "groq".to_string(),
            api_key: Some(SecretKey("sk-secret".to_string())),
            refresh: false,
        };
        assert!(
            !format!("{intent:?}").contains("sk-secret"),
            "the key must never leak through the intent's Debug"
        );
    }

    #[test]
    fn set_api_key_intent_debug_redacts_the_key() {
        for target in [
            KeyTarget::Model("groq/llama".to_string()),
            KeyTarget::Tavily,
        ] {
            let intent = Intent::SetApiKey {
                target,
                key: SecretKey("sk-secret".to_string()),
            };
            assert!(
                !format!("{intent:?}").contains("sk-secret"),
                "the key must never leak through the intent's Debug"
            );
        }
    }
}
