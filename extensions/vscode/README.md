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
  { requested_role: Contributor })` → `Catchup` + a live `Event` stream.
- **Approvals** arrive as `ToolProposed` / `ApprovalRequested` events and are
  resolved with `ResolveApproval { decision, scope: Once }`.
- **IDE context** is pushed as `UpdateIdeContext { session_id, update }`,
  debounced ≥ 300 ms client-side.
- **Resume:** the client retains only its connection and the highest ledger
  sequence seen. On disconnect it reconnects with exponential backoff and
  re-attaches with `last_seen_sequence`, so a kill/reload recovers purely via
  attach-resume.

## Commands

| Command | ID | Action |
| --- | --- | --- |
| Codypendent: Open Session | `codypendent.openSession` | Prompt for / read the session UUID, connect, focus the panel |
| Codypendent: Resolve Approval | `codypendent.approve` | Resolve an approval by UUID (Approve / Reject) |
| Codypendent: Start Run | `codypendent.startRun` | Start a run in the attached session |

Settings: `codypendent.sessionId` (auto-attach on startup when set) and
`codypendent.socketPath` (override the discovered socket path).

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
npm run build       # esbuild bundle -> dist/extension.js
```

Press `F5` in VS Code (or Cursor) to launch an Extension Development Host.

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
