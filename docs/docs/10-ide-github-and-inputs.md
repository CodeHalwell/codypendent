# IDE, GitHub, and Multimodal Integration

## IDE strategy

The daemon owns the session. IDE extensions are thin, editor-aware clients.

### Common bridge

```rust
#[async_trait]
pub trait IdeBridge {
    async fn workspace_state(&self) -> Result<WorkspaceState>;
    async fn open_documents(&self) -> Result<Vec<OpenDocument>>;
    async fn active_selection(&self) -> Result<Option<EditorSelection>>;
    async fn diagnostics(&self) -> Result<Vec<Diagnostic>>;
    async fn apply_edit(&self, edit: WorkspaceEdit) -> Result<()>;
    async fn reveal_location(&self, location: Location) -> Result<()>;
    async fn show_diff(&self, request: DiffRequest) -> Result<()>;
}
```

### VS Code and Cursor

A TypeScript extension should provide:

- side panel;
- session handoff;
- selection and open-buffer context;
- diagnostics;
- diff display;
- commands;
- approval prompts;
- terminal and test-state integration.

Cursor can share much of the VS Code codebase but requires separate compatibility testing.

### Zed

Expose Codypendent as an ACP agent. Add a small extension only for context or UI features not available through ACP.

### JetBrains

Use a Kotlin IntelliJ Platform plugin. Map application, project, and module services to Codypendent's user/workspace/repository scope.

## Unsaved buffers

The filesystem is not always the user's current truth. IDE clients send digests for dirty buffers and transfer contents only when required and authorized.

A model context entry must state whether source came from:

- committed Git revision;
- filesystem;
- unsaved IDE buffer;
- generated patch;
- agent worktree.

## Session handoff

```text
TUI session
→ user opens in VS Code
→ daemon attaches VS Code as contributor
→ relevant files and diff open
→ TUI remains observer/controller
→ agent continues without restart
```

## GitHub architecture

### Personal mode

Use local Git plus `gh` or OAuth credentials.

### Organization mode

Use a GitHub App with narrowly scoped permissions and webhooks.

Codypendent integrates:

- repositories and branches;
- issues;
- pull requests;
- reviews and comments;
- checks and Actions;
- releases;
- security findings;
- discussions and projects where useful.

## Pull-request workflow

```text
select PR/check
→ retrieve metadata and logs
→ create isolated worktree
→ investigate
→ propose patch
→ run tests
→ independent review
→ user approval
→ commit/push
→ update PR
→ publish check/summary
```

Every remote write is visible in the approval and trace systems.

## GitHub event ingestion

Webhook events are normalized into internal events. Delivery IDs provide idempotency. Signatures are verified before processing.

Events should update projections and may trigger workflows only when policy permits.

## Multimodal input

```rust
pub struct InputEnvelope {
    pub source: InputSource,
    pub blocks: Vec<InputBlock>,
    pub scope: Scope,
    pub attachments: Vec<ArtifactRef>,
}

pub enum InputBlock {
    Text(String),
    Audio(AudioArtifact),
    Image(ImageArtifact),
    File(ArtifactRef),
    EditorSelection(EditorSelection),
    CodeSymbol(SymbolRef),
    GitHubReference(GitHubReference),
}
```

### Voice

Support:

- push-to-talk;
- streaming or post-record transcription;
- local or cloud transcription policy;
- transcript review;
- deterministic voice commands;
- optional speech output.

Keep the original audio artifact where policy allows.

### Images

Accept:

- clipboard paste;
- file attachment;
- screenshot;
- IDE attachment;
- terminal image protocols when available.

Preserve:

1. original image;
2. extracted text;
3. model observations;
4. crop or coordinate references.

Do not replace the original with a textual summary.

## TUI interactivity

Ratatui and the terminal backend should support:

- mouse clicks;
- scrolling;
- pane resizing;
- expandable trees;
- menus;
- command palette;
- contextual actions;
- keyboard equivalents;
- accessible focus order.

Widgets never perform backend I/O directly. They dispatch actions into the application event loop.

## Themes

Use semantic tokens, not hard-coded colors:

```rust
pub struct Theme {
    pub surface: SurfaceTokens,
    pub text: TextTokens,
    pub status: StatusTokens,
    pub syntax: SyntaxTokens,
    pub diff: DiffTokens,
    pub agent: AgentTokens,
}
```

Ship true-color, 256-color, 16-color, monochrome, high-contrast, and color-blind-safe variants.

## Browser verification, remote control, and status projections

Browser verification produces screenshots, accessibility/DOM state, console errors, network failures and assertions.

Remote control attaches an authenticated client to the local daemon. Files, local tools and credentials remain local unless a separate runner is selected.

Shared status projections include mode, run state, plan progress, model, context use, cost, worktree, pending approvals and active agents.
