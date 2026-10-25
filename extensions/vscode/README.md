# Codypendent — VS Code / Cursor extension

An editor-aware **client** for the Codypendent daemon (Phase 3, STEP 3.5). The
extension attaches to a session over the daemon's Unix domain socket, renders the
live transcript and run state in a side panel, relays your approval decisions,
and pushes your IDE context (active file, selection, open files, dirty-buffer
digests, diagnostics revision) to the daemon.

> **Invariant: the extension never executes tools locally.** It observes the
> session, forwards editor context, and relays the user's approval decisions.
> All tool execution happens in the daemon.

## Architecture

The wire protocol is reproduced from the Rust `codypendent-protocol` crate:

| Concern | Rust source | TypeScript |
| --- | --- | --- |
| Length-prefixed JSON framing | `framing.rs` | `src/protocol/frame.ts` |
| Socket discovery | `discovery.rs` | `src/protocol/discovery.ts` |
| Envelope / Payload / Command / Event / IDE types | `envelope.rs`, `command.rs`, `events.rs`, `ide.rs`, … | `src/protocol/types.ts` |
| Connect / handshake / attach-resume / reconnect | — | `src/client.ts` |
| Editor wiring (webview, approvals, context push, diff) | — | `src/extension.ts` |
| Transcript webview | — | `src/webview/panel.ts` |
| Semantic Remote UI envelope bridge | `remote_ui.rs::UiWireMessage` | `src/remote-ui/wire.ts` |
| Atomic document/patch projection | — | `src/webview/remote-ui/store.ts` |
| Accessible React/DOM renderer | — | `src/webview/remote-ui/renderer.tsx` |
| Truthful point/region/focus registry | — | `src/webview/remote-ui/slot-registry.ts` |
| Webview capability/theme runtime | — | `src/webview/remote-ui/capabilities.ts`, `theme.ts`, `main.tsx` |

**The only module that imports `vscode` is `src/extension.ts`.** Everything under
`src/protocol/` and `src/client.ts` is pure and runs under Node, so the test
suite exercises the protocol/transport logic with no VS Code runtime.

### Wire contract (must match the daemon exactly)

- **Framing:** each frame is `[u32 big-endian payload length][JSON bytes of one
  Envelope]`. `MAX_FRAME_BYTES = 16 MiB`. The decoder rejects an oversize frame
  the moment the length prefix is readable.
- **Enums** are internally tagged with a `"type"` field and PascalCase variant
  names; unknown `type` values are ignored (forward-compatible).
- **Handshake:** connect → `ClientHello` → `ServerHello` → `Command(AttachSession
  { requested_role: Approver })` → `Catchup` + a live `Event` stream. (The
  extension both starts runs and resolves the approvals it surfaces, so it
  attaches as `Approver` — a superset of `Contributor` — not `Contributor`.)
- **Approvals** arrive as `ToolProposed` / `ApprovalRequested` events and are
  resolved with `ResolveApproval { decision, scope: Once }`.
- **IDE context** is pushed as `UpdateIdeContext { session_id, update }`,
  debounced ≥ 300 ms client-side.
- **Resume:** the client retains only its connection and the highest ledger
  sequence seen. On disconnect it reconnects with exponential backoff and
  re-attaches with `last_seen_sequence`, so a kill/reload recovers purely via
  attach-resume.

### Semantic Remote UI

The panel also hosts extension-provided surfaces produced with `@codypendent/ui`.
React executes in a confined producer runtime and emits a data-only semantic
tree; plugin JavaScript is never evaluated inside the VS Code webview. The
dedicated `Payload::RemoteUi { message: UiWireMessage }` daemon envelope is
translated at the extension-host boundary into SDK `UiHostMessage` snapshots
and patches. User input travels back as revision-bound SDK `UiRuntimeMessage`
events.

The same narrow bridge preserves the SDK's mediated state/command channel:
`subscription`, `projection`, `action`, `actionResult`, and `cancelAction` wire
messages are bounded and shape-checked independently of DOM events. A trusted
SDK adapter can use `subscribeMediatedWire` / `sendMediatedWire`; the outbound
side permits only subscription, action, and cancellation requests, while
projection updates and action results remain host-to-runtime only. This backs
`useSession`, `useRun`, `useArtifact`, and `useCommand` without exposing raw host
I/O or allowing a semantic node to manufacture a daemon command.

The graphical client implements the complete built-in primitive catalog:
layout, rich content and diffs, media fallbacks, tables/trees/graphs/charts,
feedback, navigation, forms, action surfaces, and Codypendent domain cards.
Native DOM controls provide keyboard and screen-reader semantics; focus is
restored across patch revisions and shortcut handling is centralized. Unsupported
capabilities resolve through the SDK fallback tree before rendering.

Security and recovery boundaries are deliberately narrow:

- snapshots and patches are schema/size checked and applied atomically;
- stale or invalid updates keep the last good revision visible and request a
  full resync;
- props remain inert JSON data, Markdown never injects HTML, external media is
  blocked, and links use an explicit protocol allowlist;
- the webview CSP is `default-src 'none'` with nonce-bound scripts and styles;
- webview view state caches bounded last-good snapshots only as a reload aid;
  the daemon remains authoritative and receives resync requests after restore;
- semantic theme tokens are validated and mapped consistently to governed CSS
  custom properties (`text.*`, `surface.*`, `status.*`, `focus.*`, `spacing.*`);
- the advertised contribution list is derived from a concrete 22-point slot
  registry. Sidebar, navigation, primary, transcript, composer, setup, status,
  and overlay regions have separate layout/lifecycle behavior; lower stacked
  interactive overlays become inert while the topmost owns focus;
- each document is isolated behind host-native recovery UI. Retry requests an
  authoritative resync, Disable requires a modal confirmation before revoking
  the owning plugin, and Report writes bounded details to the Codypendent output
  channel. Healthy sibling documents stay mounted.

Set `codypendent.remoteUi.terminalFallbackPreview` to show the deterministic
minimal-terminal projection below graphical contributions. This is useful for
extension development, accessibility review, and checking graphical/terminal
parity.

## Commands

| Command | ID | Action |
| --- | --- | --- |
| Codypendent: Open Session | `codypendent.openSession` | Prompt for / read the session UUID, connect, focus the panel |
| Codypendent: Resolve Approval | `codypendent.approve` | Resolve an approval by UUID (Approve / Reject) |
| Codypendent: Start Run | `codypendent.startRun` | Start a run in the attached session |

Settings: `codypendent.sessionId` (auto-attach on startup when set) and
`codypendent.socketPath` (override the discovered socket path).

## Continue from the desktop or terminal

Install this extension in VS Code or Cursor, then choose that editor from the
desktop workspace menu or run `codypendent open <session> --in vscode` (use `cursor`
for Cursor). The editor receives the current repository, daemon socket and
session together, so there is no UUID to copy. Accept **Open Session** to attach;
an already attached matching session is reused. A repository opened in a new
window continues the approved handoff after the workspace is trusted.

Opening a line from a desktop patch review also carries the patch's originating
run. The editor opens that run's worktree at the selected line while keeping the
original session attached. Missing or retired checkouts produce an error instead
of opening an unrelated copy. Handoff links contain local paths and identifiers,
never credentials. The extension checks socket ownership, checkout provenance
and file containment before opening them.

## Cursor compatibility

Cursor is a VS Code fork and loads this extension unchanged — it uses the same
`vscode` extension API, the same activation events, the same webview view API,
and the same `vscode.diff` command. Notes:

- `engines.vscode` is `>=1.90.0`; Cursor tracks a recent VS Code baseline, so the
  APIs used here (webview views, `TextDocumentContentProvider`, diagnostics
  events) are available.
- Only stable API is used — no proposed API — so no `enabledApiProposals` is
  needed and the extension installs from a `.vsix` in either editor.
- The extension talks to the daemon over the daemon's Unix socket, resolved
  identically to the daemon (`CODYPENDENT_SOCKET` → `CODYPENDENT_DATA_DIR/run` →
  `XDG_RUNTIME_DIR/codypendent` → platform data dir). Cursor inherits the same
  environment, so discovery matches.

## Develop

```bash
npm install
npm run typecheck   # tsc --noEmit (strict)
npm run lint        # eslint
npm test            # vitest (pure protocol + client, no VS Code runtime)
npm run build       # esbuild bundles -> dist/extension.js + dist/webview.js
```

`npm install` also builds `@codypendent/ui` (the local `file:../../sdk/ui`
dependency this package uses) via that package's own `prepare` script — no
separate `cd ../../sdk/ui && npm ci && npm run build` step needed first.

Press `F5` in VS Code (or Cursor) to launch an Extension Development Host.
The test suite also runs jsdom structural visual conformance against the SDK's
shared loading/empty/error/long-content story across public points, themes, and
viewports. It asserts semantic roles, focus arbitration, recovery actions,
long-content containment, and truthful capability advertisement; use real
Extension Development Host screenshots for release-candidate pixel review.

## Smoke-test checklist

Run against a live daemon (a session must already exist; the extension never
creates or executes anything — it attaches to a session id).

1. **Discovery / connect.** With the daemon running, set `codypendent.sessionId`
   (or run **Codypendent: Open Session** and paste a session UUID). The panel's
   status badge should move `connecting → handshaking → attaching → attached`.
2. **Catch-up + transcript.** On attach, prior events render in the panel
   (session title, notes, run state). New events stream in live.
3. **Run state.** Start a run in the daemon (or **Codypendent: Start Run**); the
   panel header shows the run id and its `RunState` transitions.
4. **Approval round-trip.** When the agent proposes a tool / approval, an
   information message with **Approve / Reject** appears (and an approval card in
   the panel). Choosing one sends `ResolveApproval` and the daemon emits
   `ApprovalResolved`; the card updates. Confirm no tool ran in the editor —
   execution is the daemon's.
5. **IDE context push (debounced).** Switch the active editor, move the
   selection, and edit an unsaved buffer. Within ~300 ms of the last change a
   single `UpdateIdeContext` should be sent (verify daemon-side): `active_file`,
   `selection`, `open_files`, `dirty_buffers` (path + SHA-256 + byte length), and
   an incrementing `diagnostics_revision`.
6. **Change-set diff.** On a `PatchProposed` event a `vscode.diff` view opens for
   the change set.
7. **Resume after reload.** Reload the window (or kill/restart the daemon). The
   client reconnects with backoff and re-attaches with `last_seen_sequence`; the
   transcript is recovered from catch-up — the extension keeps no session state
   of its own.
8. **Cursor.** Repeat steps 1–7 in Cursor; behaviour should be identical.
9. **Remote UI.** Mount a semantic contribution and verify its snapshot,
   incremental patches, form/keyboard events, theme, and terminal fallback.
   Reload the webview and verify it displays the last-good tree while requesting
   an authoritative resync.
10. **Point placement and recovery.** Mount contributions in sidebar, transcript,
    composer, status, and an overlay; verify each occupies its host region and
    only the topmost interactive overlay receives focus. Force one document to
    fail, then exercise Retry, Report, and confirmed Disable without disturbing
    a healthy sibling.
