# Native workspace and Apple release preparation

Work on 4 October 2026 builds on the existing uncommitted September review.
No release was published, and no Apple identity or repository secret was changed.

## Desktop

- Compact workspace bar with repository context and installed application hand-off.
- Persistent System/Light/Dark appearance, sidebar visibility and keyboard focus mode.
- Native workspace menu shares navigation and composer actions with the React client.
- Branded welcome screen with repository, model and session entry points; connection
  diagnostics are expandable instead of dominating the first screen.
- Eight secondary views load on demand. A closed command palette renders nothing.
- The development server and Tauri URL both use IPv4 loopback explicitly. Live
  testing found another Tauri app listening on IPv6 `localhost:1420`; mixed
  address families could otherwise put that app's frontend inside this shell.
- New app icon, macOS ICNS family, lightweight sidebar/favicon assets and matching
  monochrome VS Code activity icon. Regenerate with `npm run icons` in `apps/desktop`.

The final production Vite entry changed from **498.23 kB to 449.31 kB** (9.8% smaller),
or **134.78 kB to 128.82 kB gzip** (4.4% smaller), including patch review, the
stable connection recovery display, launch-event handoff, and bounded transcript
history and native composer focus (`index-CsoSMppT.js`).
These are bundle measurements, not native launch-time measurements.

The existing full-App streaming harness measured 4.46 ms/token with 50 history
entries and 4.81 ms/token with 400, for 300 streamed tokens in jsdom. These are
current component measurements, not a before/after claim or a WebKit frame-time
measurement.

## Terminal

- `-C` / `--working-directory` opens a checkout directly from an IDE or terminal launcher.
- Option word editing, forward delete and Shift-Enter multiline composition.
- Bounded, single-pass clipboard normalisation; terminal titles are deduplicated
  and restored on exit, hyperlink rendering avoids per-cell URL copies, and
  unfocused terminals stop continuously redrawing animations.
- Online agent catalogue refresh moves off the workspace startup path.
- Browsed patch cards support Alt-N/P for hunks and Alt-Shift-N/P for changed
  files, show the selected section and its position, and return to the complete
  preview with Alt-Enter. Navigation uses the existing bounded preview; the
  full-artifact truncation notice is retained.
- Alt-O opens the reviewed file/hunk in the originating run's checkout, carrying
  the attached session into VS Code/Cursor and preserving the composer draft.
  CLI and TUI share the URI builder, canonical path checks and durable lease
  lookup; nested repository launches use the existing repository anchor.

A short optimised Rust harness on this Mac compared the exact old/new
normalisers over 100 iterations of a 9,000,000-byte mixed Unicode/CRLF clipboard:
**10.252 ms to 0.306 ms per paste** (33.5× faster), with identical 65,535-byte
output. The production-path benchmark is preserved as
`cargo bench -p codypendent-tui --bench frame -- tui/paste --quick`; that release
benchmark was not run during this session.

The repeatable PTY smoke driver is `uv run crates/cli/tests/pty_smoke.py --binary
target/debug/codypendent`. It uses disposable local state by default and never
submits a model prompt.

## IDE session handoff

VS Code and Cursor receive the existing session, daemon socket and repository
through an extension URI, including when the editor was already running. The
extension validates the request, opens the matching checkout, and replays that
session without asking for a UUID. It waits for workspace trust before connecting.
Desktop and TUI patch review carry the originating run and one-based file line through
the handoff; native lookup resolves that run's durable workspace lease. A retired
checkout is reported explicitly. File traversal and symlink escapes are refused.
The separate **Open repository in…** menu opens the selected base checkout and
preserves the session, including after a completed run retires a clean worktree.

## Apple distribution

The release workflow stages the matching CLI, UI worker and sealed Node runtime
inside the app before signing. The launcher prefers the bundled runtime over a
global installation. Packaging validates version, architecture and runtime seal;
the distribution verifier checks actual Developer ID authority, CS_RUNTIME flag,
secure timestamp, stapled notarization and Gatekeeper acceptance.

See [the release procedure](desktop-release-smoke.md) for staging, signing and
the separate Mac App Store requirements. Signing and notarization have not run
in this session. The native target remains Apple Silicon macOS; no Intel or
Linux GUI result is inferred from testing this machine.

## Verification

Live browser checks exercised appearance and sidebar persistence across reload,
command search with the sidebar hidden, on-demand Analytics loading, welcome-card
navigation and the layout at 720 × 700.

The packaged Apple Silicon app ran its embedded frontend at `tauri://localhost`
with the Vite server stopped. With an isolated data directory, minimal `PATH` and
no binary override, Connect workspace launched the CLI inside the bundle. Native
repository selection and reconnect worked; Open in opened the exact checkout in
both Visual Studio Code and Finder, verified in their application windows. Native
Cmd-O, Cmd-L, Cmd-K, Cmd-B and Cmd-comma actions worked, including returning focus
to the composer from an open command palette.

The native model picker tested and selected the loopback fixture. A submitted
objective completed through the bundled daemon, displayed the streamed fixture
response, and showed its synthetic usage of 10 input / 24 output tokens. The
fixture made no external model request. The helper in
`apps/desktop/scripts/smoke-provider.py` reproduces this workflow.

Live checks caught a misleading browser-preview message on first paint and a
brief contrast mismatch when switching to Light. Initial connection state now
starts as Connecting; theme colour changes no longer animate button backgrounds.
The first debug daemon boot took 16.455 seconds on this loaded Mac, including
installed-skill registration and sealed-runtime validation. Native startup waits
up to 30 seconds with a live readiness probe, preserving integrity checks.

The final embedded rebuild was launched again with the daemon stopped. It kept
Connecting visible during startup and reached Connected without the earlier
false start failure. Repository, selected model, appearance and sidebar choices
survived restart; reopening the saved session restored its response and usage.
The Light switch showed the correct repository-button contrast immediately.
Testing restored System appearance and the visible sidebar, then stopped the
isolated app, daemon and loopback provider.

The unsigned self-contained test app is at
`apps/desktop/src-tauri/target/debug/bundle/macos/Codypendent.app`.

The rebuilt CLI passed the repeatable PTY driver using isolated local state:
onboarding and skip confirmation, Unicode bracketed paste, Option word movement
and deletion, Shift-Enter multiline composition, searchable F1 help, Escape clear
and Ctrl-Z restore, F2 layout switching, and resizing from 110 × 32 to 40 × 12
and back without losing the draft. Ctrl-C detached with exit code 0. Raw terminal
output confirms one application title write, one title-stack save, and title and
keyboard-protocol restoration on exit. All 13 checks passed; evidence is in
`/tmp/codypendent-tui-apple-evidence/` (`result.json`, `terminal.raw` and six text
screen captures). No model prompt was submitted.

After the native app launched its bundled daemon, the rebuilt TUI also attached
successfully to `/tmp/cody-apple-gzsIL6/daemon.sock` using the same repository and
model configuration. Its 12 applicable checks passed and it detached without
stopping the native-owned daemon. This second run is recorded in
`/tmp/codypendent-tui-shared-daemon-evidence/`.

The TUI also attached to the actual model session
`01a107ef-ff6c-74c2-aed0-a2851fe2da9f` using the repeatable driver's new
`--inspect-only` mode. Five view-only checks passed, including replay, layout
switching and clean detach; no draft or objective was submitted. Evidence is in
`/tmp/codypendent-tui-real-session-evidence/`.

The rebuilt TUI then reviewed the actual three-file patch from completed session
`01a10815-59ac-7010-aeff-cfb60f3a59f5`. Fourteen PTY checks passed: exact
changed-file search, next/previous file and hunk, return to the complete bounded
preview, workspace view and clean detach. The daemon stayed running and no
objective was submitted. Raw bytes, result JSON and file/hunk screens are in
`/tmp/codypendent-tui-patch-navigation-evidence/`. Reproduce using the driver's
`--review-patch 'README.md cli.py words.py'` option with that fixture's data,
socket and repository paths.

The final TUI-to-editor check passed **18 PTY checks** against that same completed
session. With an unsent draft still in the composer, Alt-O on the second hunk
opened the retained run checkout's `cli.py` at **line 1, column 1** in the actual
VS Code application. Its Codypendent panel showed **attached**, the Completed
run and the original coding history; the extension log confirms the exact same
session and socket. The one-time URI and Open Session confirmations were accepted,
and the pre-existing Typhon files and settings were untouched. The TUI preserved
the draft and detached cleanly without submitting it. Evidence is in
`/tmp/codypendent-tui-ide-app-handoff-evidence/`; reproduce with the same driver
and fixture arguments plus `--open-reviewed-source cli.py`. On macOS this uses
the installed editor application before a PATH launcher, avoiding ambiguous
`code` commands belonging to another installed editor.

The repository-built extension VSIX was installed and used in both editors.
VS Code opened a new window for the fixture repository with `cli.py` at
**line 2, column 1**; the Codypendent panel visibly showed **attached**, the
completed run and its original objective/tool history. The extension output log
confirms that it attached to the same session listed above. The pre-existing
Typhon window and files were preserved; no Codypendent extension was previously
installed in the normal VS Code profile. Cursor also opened the correct file and
line and logged the same successful attachment, although its first-run sign-in
overlay covered the conversation panel. No Cursor login was performed.
Logs and result records are in `/tmp/codypendent-ide-handoff-evidence/`; the
tested VSIX is `/tmp/codypendent-0.14.0-handoff.vsix`.

Extension typechecking, bundling and 27 focused tests passed (16 URI/path/socket
and checkout checks, plus 11 existing editor-action tests). These app checks used
the base checkout because the initial run made no edits and its clean worktree
had been retired. A later native patch-gutter check opened the retained coding
run's `cli.py` at line 27, column 1, with its completed session and history visible
in VS Code; see [coding acceptance](2026-10-04-coding-acceptance.md#native-recovery-and-source-navigation).
Five focused native environment tests also passed, covering literal application
arguments, unknown application refusal, source-file containment, encoded session
and line handoff, and originating worktree/session ownership.

Focused Rust checks passed: five terminal-editing/catalogue integration tests,
six terminal unit tests, three patch-navigation reducer/render tests, two
source-handoff reducer/parser tests, two CLI URI/lease/containment tests and five
VT100 rendering/snapshot tests. The two help
snapshots were updated and inspected. Actual `skill new --help` includes both
the global `--working-directory` and existing skill `--directory` flags. An
invalid launch directory exits immediately with a descriptive error. Source
diff whitespace checks pass; terminal snapshots retain intentional trailing
spaces that represent screen width.

Focused frontend tests, the production build, packaging regressions, release
workflow security checks and static documentation counts were run. Full workspace
tests and model evaluations were not run for this change.

## Final installed release-candidate verification

The final `index-CsoSMppT.js` app was copied from its read-only mounted DMG to a
disposable Applications directory; the DMG was unmounted before launch. With
`PATH=/usr/bin:/bin:/usr/sbin:/sbin`, native **Start** launched the bundled daemon
and connected using protocol **1.10**. The 25,004-item fixture initially showed
the bottom of its latest history page, with notes **24983–24999** visible.

**Cmd-L → paste** focused the composer without clicking it. From Session Library,
native **Workspace → Focus Composer → paste** returned to the session and kept
the existing draft. **First 1–200 → Newer 201–400 → Latest 24,805–25,004** also
retained the draft. It was cleared without submitting an objective. The GUI
**47550** and isolated bundled daemon **52411** were quit/stopped and confirmed
absent. Native accessibility reports the bounded message range; the independent
200-row DOM count was verified in the separate browser fixture.

The final focus correction passed **26 focused workspace/focus/palette tests**,
including held animation frames, palette/shortcut cleanup, repeated native
actions, draft retention and blocking confirmations. TypeScript/Vite passed.
Evidence: `target/apple-release-candidate-2026-10-04/evidence/native-installed-app-review.json`.
Earlier hashes and measurements above describe their recorded builds. Apple
certificate-backed signing/notarization and clean-Mac Gatekeeper verification
remain outstanding.
