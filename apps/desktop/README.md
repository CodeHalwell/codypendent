# Codypendent desktop

The graphical client for `codypendentd`: a Tauri shell around a React app.
The shell's Rust side owns the daemon socket and the local configuration files;
the webview is a projection of daemon state and never touches the filesystem or
the network itself. The terminal client (`codypendent`) and this app attach to
the same daemon, so a session started in one is live in the other.

## First launch

1. Apple release bundles include the matching CLI, worker launcher, Node
   runtime and Monty Python sandbox. Development builds can use an installed `codypendent` on `PATH`,
   in `~/.cargo/bin`, `~/.local/bin`, `/usr/local/bin`, or `/opt/homebrew/bin`,
   or set `CODYPENDENT_BINARY` to the build to test.
2. Open the app. It starts its local service automatically when needed and
   connects as soon as it is ready. Reopening the app reuses the running service;
   a dropped connection restarts it if necessary. Startup failures show recovery
   controls and details from `<data dir>/logs/daemon.log`. Development builds can
   select a daemon with `CODYPENDENT_BINARY=/path/to/codypendent`.
3. Installed coding agents, running Ollama/LM Studio models and existing
   provider credentials are discovered in the background. Available profiles
   are added automatically; an existing model selection stays selected. Setup
   opens only when discovery cannot find a usable configuration. **Models →
   Scan again** checks for tools installed or started since launch.

The sidebar footer shows which daemon you are attached to (`daemon 0.14.0 ·
protocol 1.4 · desktop 0.14.0`) and calls out a version mismatch, which is the
usual sign of a daemon left running across an upgrade.

## Native workspace integration

The desktop keeps chat, search and recent sessions in the sidebar, with Councils,
Workflow, Workspace and Configuration available in collapsible groups. Filter
recent chats by title, or use the command palette to reach every surface. The
centred conversation and composer share Codypendent's ivory, petrol and gold
palette in light and dark appearances. Model and mode controls in the composer
open the existing configuration screens; run, approval and queue controls retain
their existing behaviour. Smaller windows use a dismissible sidebar drawer.

The workspace toolbar's **Open in** menu detects installed editors and opens the
selected checkout in VS Code, VS Code Insiders, Cursor, Zed, Windsurf, RustRover
or Antigravity IDE. On macOS it also opens the checkout in Finder or Terminal.
No editor is launched during discovery. Each hand-off revalidates the saved
checkout; terminal paths are passed as literal arguments, never shell commands.

The native **Workspace** menu provides Open Repository (`⌘O`), Focus Composer
(`⌘L`), Command Palette (`⌘K`), Toggle Sidebar (`⌘B`) and Settings (`⌘,`).
Standard macOS Edit, Services, Window and application menus remain available.

## Models, providers and keys

Automatic discovery uses installed adapters and existing sign-ins. It does not
install an agent, open a login flow or send a generation request. Slow or failed
agent handshakes are bounded and remain retryable. Your configuration and model
choice are shared with the CLI and preserved by the same locked writer.

**Models** lists every `[[model]]` in `models.toml` with two badges: credential
presence (stored, from an environment variable, or missing) and readiness. The
readiness badge comes from the running daemon's `ProbeModel` response. Opening
the picker resolves configuration and credentials without calling provider
endpoints. **Test** explicitly asks the daemon to probe the model. An older
daemon is reported as unsupported, and an unreadable configuration is reported
as an error. **Use** pins a model for the next run; that choice and the mode
persist across launches.

**Providers** lists the built-in catalog, your `providers.toml`, cached ACP
registry entries, and locally discovered agents. **Refresh agent catalog**
fetches current official registry metadata. Select an available coding agent,
review the selected repository, then **Connect agent**. This installs its pinned
adapter if necessary, completes a handshake, and saves the default profile and
all advertised models. A failed handshake saves no profiles. The legacy community
Antigravity bridge requires explicit acceptance of its account and terms risk;
this does not apply to Google's installed official Antigravity CLI.
Model profiles can be removed under **Models**. CLI, TUI and desktop updates
share the same locked, comment-preserving model-file writer.

**API Keys** shows presence only. A key you enter goes one way, into
`auth.json` at mode `0600`; nothing in the app can read it back.

## During a run

- The composer strip always names the model and mode staged for the next run
  and, while a run is live, what it is doing: `working…`, `running shell.run…`,
  `waiting for your approval`, or `retrying (2/5) · provider is overloaded ·
  next attempt in 8s` when the provider is backing off.
- The transcript shows a pulsing working row between visible updates, policy
  denials (`Blocked by policy: …`), budget warnings, and the measured usage
  once the run ends (`1,234 in · 567 out · $0.0034`; an unmeasured dimension is
  absent, never zero). `partial` identifies measured subtotals, while older
  records show `coverage unknown`. Tiny nonzero costs retain six decimals.
- An approval card shows the complete proposed action, including command
  arguments, working directory, environment and paths when supplied, plus the
  risk assessment. Supported actions offer Approve and Reject; an action type
  this client cannot interpret explains the limitation and permits rejection
  only. A question card offers the
  agent's options (radio or check boxes), a typed answer where the prompt
  allows one, and a rejection with feedback; the answer is sent as the
  protocol's own `ResolveQuestion`.
- Decisions show sending, refusal, and accepted states. A pending or accepted
  decision disables both choices, including after leaving the view and returning.
  Acceptance waits for the daemon's resolution event before retiring the card.
  Refused answers and rejection feedback stay available for retry; keyboard focus
  returns to the sending button. Reissued questions reset their drafts even when
  the prompts are identical, and reconnect retires obsolete command feedback.
- Inbox **Review** opens the session that owns an approval or question and
  focuses the full request. Missing session information and failed attachments
  produce an actionable error. **Acknowledge** and **Dismiss** organize the inbox;
  they do not decide requests. Each entry allows one pending action, displays
  refusals locally, and ignores refresh or mutation results from older connections.
- A failed run is a red card with the cause in plain words, the sanitized
  provider response folded underneath, and the next step as a button: **Open
  API keys** for an authentication refusal, **Choose a model** when none is
  configured, **Retry** with the same objective.
- New objectives and queued follow-ups clear only after acceptance. Refusals
  and connection errors preserve the draft, including edits made while a request
  is pending or after navigating away and back. Enter used to confirm IME text
  does not send the prompt.

## Live extensions

The desktop negotiates Remote UI capabilities after confirming a session
attachment. The daemon supplies verified contribution placement, snapshots,
patches and disposal messages; the renderer sends interactions and recovery
requests back over the same session-bound connection. A missed revision keeps
the last valid document visible and requests a fresh snapshot. Reconnect and
session switching discard the previous host and renegotiate. Extension identity
comes from broker evidence, never the document's own metadata.

## Agent tools and interactive results

The native AG-UI agent adapts the existing daemon's run, text, reasoning, tool
and decision events. The composer still submits to the same daemon, and its
durable journal owns the conversation. No additional model service is needed.

In Build mode, native models receive the desktop tool definitions directly.
Installed ACP agents receive the same tools through a per-run local MCP server.
The agent can read workspace context, find chats, inspect models and knowledge,
open a page or source file, and prepare a draft. Existing coding tools remain
available. Mutations continue through the daemon's normal policy and approvals.

`generateSandboxedUi` creates an interactive answer in the conversation.
`desktop.defineTool` creates a reusable `desktop.custom.<name>` helper with an
object JSON Schema and declared desktop capabilities. New computational helpers
can use `language: "python"`: their function body receives `args`, calls
`host_call(name, args)` for declared capabilities and returns JSON. Python runs
in the bundled Monty subprocess with 32 MiB memory and a five second total limit.
Existing JavaScript async function bodies remain supported when language is
omitted or `"javascript"`. Interactive HTML results retain their browser sandbox.
Helpers have no direct filesystem, native bridge or network access. Declared host calls
are separately authorised by daemon policy. Tool results return to the same
model call and are stored as verified artifacts, so reopening a chat restores
the result. **Workspace → Agent tools** shows saved definitions, schemas,
versions and capabilities, with confirmed version-checked removal.

See [the integration and acceptance record](../../docs/reviews/2026-10-19-workspace-uplift.md)
for the broker, ACP bridge and AG-UI event contracts.

## Patches and exports

Expand a patch card and choose **Load full patch** to read its verified artifact.
The panel shows up to 1 MB of text and explains when the view is truncated.
**Save file…** opens the native save dialog and saves the complete artifact.
Analytics and session exports offer the same save action once their export
artifact is ready. Cancelling the dialog is reported as cancellation; errors
keep the action available for another attempt.

The shell streams Save downloads in bounded chunks into a private file beside
the destination. It verifies the complete length and SHA-256 and syncs the file
before replacing the destination atomically. A failed fetch or write preserves
an existing export. Save can exceed the preview reader's 64 MiB limit without
buffering the whole file. The browser cannot supply a save path; saving requires
the connected desktop shell.

A changed artifact reference or connection clears old loaded content and Save
notices. Late results cannot appear under a newer artifact, even when its ID is
unchanged. Load and Save retain keyboard focus while pending and restore their
button after completion or refusal.

## Queued follow-ups

Queue edits retain their draft after refusal and while the panel is closed or
another desktop view is open. Save, Send next, and Remove share a pending guard
for each prompt; another prompt can still be changed independently. Acceptance
closes only the draft that was submitted, so typing further changes while Save
is pending does not lose them. The displayed queue changes only when the daemon
reports its new contents.

Edit focuses the text field. `⌘Enter` / `Ctrl-Enter` saves and `Esc` cancels;
neither shortcut interrupts an IME composition. Pending actions retain focus on
their row, and refusal restores the triggering control unless focus was moved
elsewhere. Session switches discard the previous session's editor and feedback;
late replies from retired connections or attachments cannot change the new
session's queue error.

## Keyboard

`⌘K` / `Ctrl-K` opens the command palette from anywhere, `/` does the same when
you are not typing in a field, and `Esc` closes the topmost overlay or goes
back to the previous view. The palette's **Keyboard shortcuts** entry lists the
rest. Every button has a keyboard equivalent, keyboard focus is always visible,
and the app honours `prefers-reduced-motion`.

## Theme

Colours are semantic tokens in `src/theme.css` (`--cody-bg`, `--cody-text`,
`--cody-accent`, `--cody-danger`, …), following the terminal client's theme
groups. The dark palette is the default; a light palette follows the OS
preference (`prefers-color-scheme: light`) or an explicit `data-theme="light"`
on the document root.

Components reference tokens only, never a colour literal: text on a solid
accent or status button is `--cody-on-accent`, tinted panels pair a `*-bg`
token with its text token (`--cody-warning-bg` with `--cody-warning`), and
`test/theme-tokens.test.ts` fails the build if a hex literal returns or a
component uses a token that one of the palettes does not define. To add a
colour, add the token to all three blocks in `theme.css`.

## Development

```bash
npm ci
npm run desktop:dev
```

The development command builds the linked SDKs and matching CLI/worker before
opening the hot-reload shell. The shell starts its service automatically.
Native desktop builds require Rust 1.96 or newer; shared libraries and the CLI
retain their existing requirement. Tauri dev/build hooks use Python 3 to prepare
the pinned Monty worker, verify its release checksum and bundle it automatically.
Packaged users need no separate Python, Monty or Node installation.
`npm run check`, `npm run build` and scoped `npx vitest run test/<file>` remain
available separately. Linux needs the Tauri platform dependencies.

The Rust side is its own Cargo workspace (`src-tauri/`), checked, linted and
tested separately: `cargo test --manifest-path src-tauri/Cargo.toml`.
For direct Cargo builds, first run
`python3 scripts/prepare-monty-runtime.py --output ../../target/debug` from this
directory. `npm run tauri:build` prepares the release worker and Tauri sidecar;
Apple staging verifies and signs the bundled worker with the other app binaries.

## Knowledge surfaces

The **Skills**, **Memory**, **Docs** and **Plugins** views read through one
typed transport (`src/components/knowledgeTransport.ts`), which the shell
implements in two halves. The lists — registry items, memories, learnings,
documents — are the shell's own read of the daemon's SQLite database
(`src-tauri/src/knowledge.rs`), opened read-only and mapped to cards exactly
as the terminal client's harness maps them, so both clients show the same
facts; before the daemon has ever run there is no database, and the panel says
so rather than showing an empty list. The mutations — correcting or forgetting
a memory, creating, editing and publishing a document, the Remote UI plugin
lifecycle — are protocol commands on the live connection (`daemon.rs`), and a
document edit is composed as the TUI composes it: lease the block, apply the
edit, release the lease. Memory commands are scoped to the selected repository,
so they ask for one when none is selected.

Outside the shell (a browser tab, a test) there is no transport; the four
views then render an explicit unavailable panel naming the commands they need
and wear a "not in this build" badge in the sidebar and palette.

### Analytics and session recovery

Analytics can load additional pages without changing the original time window.
A failed page preserves the loaded measurements for retry; changing filters or
reconnecting retires old query/export responses. Metric cards label partial
bucket coverage, and tiny nonzero costs remain visible.

Session lifecycle actions show a pending state and reject duplicate sends.
Rename/delete dialogs retain failures for retry; rename preserves the typed
title. Keyboard focus stays inside pending dialogs and returns to the draft or
Cancel after a refusal. Deleting a row returns focus to search. A successful
mutation refreshes search results and
invalidates old cursors. Session search also offers an explicit retry after a
failed request.
