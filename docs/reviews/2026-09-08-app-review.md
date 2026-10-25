# Codypendent app review and improvement pass

Reviewed 8 September 2026 (Europe/London). Baseline: freshly fetched
`origin/main` at `088115e`, following the merge of PR #136. Work is on
`codex/app-review-2026-09-07`; the checkout was clean before switching.

This pass improves the everyday path from starting a run to reviewing its
actions and taking its output away. It also checks the shared daemon and SDK
contracts behind the CLI, TUI, and desktop. The findings below distinguish
implemented changes from release checks that still require platform testing.

## Findings addressed

| Priority | Finding at the baseline | Implemented behavior |
| --- | --- | --- |
| High | `run` required JSONL even for a person reading a single answer. | Readable streaming output is now the default. Answer text goes to stdout; session identity, tool progress, retries, waiting states, and terminal diagnostics go to stderr. JSONL remains explicitly selectable. |
| High | Queuing a desktop follow-up discarded the asynchronous acceptance result and could erase a refused draft. | Runs and queued follow-ups share acceptance handling. Refusal and transport errors preserve the raw draft; an accepted request clears only the draft it submitted. Edits survive pending requests and navigation away and back. |
| High | Approval summaries omitted evidence. The TUI did not explain GitHub mutations or checkpoint restores; desktop summaries dropped fields such as execution environment and working directory. Unknown action types could still be approved. | Desktop cards retain and display the complete action and supplied risk. TUI and accessible views share action-specific details, including restore destinations and GitHub targets. Unsupported action types permit rejection only. |
| Medium | Desktop patches and exports ended at artifact IDs, leaving users without a way to retrieve the actual result. | Patch cards load the verified artifact on demand. Patch, analytics, and session export surfaces offer a native Save dialog. Save cancellation, failure, and success are distinct outcomes. |
| Medium | The TUI's long shortcut reference required scanning and scrolling through every row. | Help now filters shortcut names, descriptions, and mouse actions as the user types, with bounded Unicode input, clear/edit controls, paging, and matching accessible commands. |
| Medium | Enter could send a desktop prompt while the user was confirming an IME candidate. | Composition events and the legacy composition key code are ignored by the submit handler. |
| Medium | The desktop startup bundle included the extension renderer even when no extension document was mounted. | The extension host and its styles load lazily after a confirmed daemon attachment. Broker delivery, interaction, revision recovery, and disposal are now connected. |

### CLI behavior and compatibility

`codypendent run --objective "Explain the failing tests"` now works without an
output flag. Whitespace-only objectives are rejected before connecting to the
daemon. Readable rendering filters to the run this command started and omits
thought deltas; a sibling run cannot contaminate its answer or end its stream.
A completion summary is used when no answer text was streamed.

The original JSONL envelope stream, catch-up replay, terminal usage drain, and
exit codes remain: completed `0`, failed `2`, cancelled `130`. `--json` remains
an alias for `--jsonl`. Ctrl-C detaches the client and tells the user the daemon
continues. Readable output strips terminal controls, and diagnostic messages
use the shared TUI failure sanitizer described below. Unstructured model text
is not a guarantee of complete secret detection.

Implementation: [main.rs](../../crates/cli/src/main.rs),
[commands.rs](../../crates/cli/src/commands.rs),
[stream.rs](../../crates/cli/src/stream.rs).
Regression coverage: [jsonl_it.rs](../../crates/cli/tests/jsonl_it.rs), including
Unicode output, terminal controls, thought/sibling filtering, completion-summary
fallback, and failed/cancelled dispositions alongside the existing JSONL cases.

### TUI and accessible operation

Help owns its input mode, so typing a search cannot accidentally execute a
normal-mode shortcut or change the composer draft. Terms match without case
sensitivity and all terms must occur. Backspace and Ctrl-W edit; Ctrl-U clears;
arrows, Page Up/Down, and Home/End navigate; Esc, `?`, and F1 close. Paste is
bounded to 256 characters. The accessible client offers the same filtered
reference and explicit editing/navigation commands.

Approval rendering now shares its evidence projection between full-screen and
accessible views. The reducer itself refuses approval of an unsupported action,
so keyboard and mouse paths cannot bypass a disabled affordance. Detail and risk
text are sanitized at rendering time too, covering recovered state as well as
live events. GitHub evidence includes repository and summary; restore evidence
includes run, turn, worktree, commit, and the consequence of discarding later
changes.

Implementation: [approval.rs](../../crates/tui/src/approval.rs),
[input.rs](../../crates/tui/src/input.rs), [reduce.rs](../../crates/tui/src/reduce.rs),
[render.rs](../../crates/tui/src/render.rs), and
[accessible.rs](../../crates/tui/src/accessible.rs).
Regression coverage: [approval_review.rs](../../crates/tui/tests/approval_review.rs),
[help_search.rs](../../crates/tui/tests/help_search.rs), and reviewed narrow/wide
VT100 snapshots. Tiny and zero-sized terminal cases remain covered.

### Desktop completion and recovery

The composer holds a synchronous submission guard while awaiting acceptance.
It remains editable, compares the submitted raw draft against the current draft,
and lets the lifted draft owner perform that comparison after a view unmounts.
The textarea has an explicit accessible label for objectives and follow-ups.

Patch events retain the full artifact reference, including integrity metadata.
Loading a full patch happens only on request. The text view is bounded to 1 MB
and handles a UTF-8 character cut at the preview boundary without corrupting the
display. The reload control stays mounted so loading does not remove the
focused button. Full bytes remain available through Save.

The Rust bridge obtains the destination from a native dialog; web content cannot
provide a writable path. The shared transfer streams into a private staging file
alongside the destination. Only complete length and SHA-256 verification followed
by a successful disk sync permits atomic replacement. Failed verification or
writing cannot truncate an existing export. Dialog cancellation does not claim a
file was saved. Save can stream beyond the desktop reader's 64 MiB assembly cap;
the UI text display remains bounded to 1 MB. Daemon limits still apply.

Desktop approval cards display structured action evidence and supplied risk,
including after compact snapshot recovery. An unsupported action cannot be
approved, and disconnected controls are disabled.

Implementation: [Composer.tsx](../../apps/desktop/src/components/Composer.tsx),
[ArtifactActions.tsx](../../apps/desktop/src/components/ArtifactActions.tsx),
[Transcript.tsx](../../apps/desktop/src/components/Transcript.tsx),
[daemonState.ts](../../apps/desktop/src/daemonState.ts),
[bridge.rs](../../apps/desktop/src-tauri/src/bridge.rs), and
[artifacts.rs](../../apps/desktop/src-tauri/src/artifacts.rs).
Tests cover acceptance/refusal, edits during pending requests, navigation,
IME handling, approval evidence and unknown actions, artifact integrity errors,
UTF-8 truncation, save cancellation/failure/success, and analytics exports.

### Measured bundle change

Using the same local toolchain and Vite production build:

| Asset | Baseline | Updated |
| --- | ---: | ---: |
| Main JavaScript | 536.93 kB | 498.23 kB |
| Main JavaScript, gzip | 146.38 kB | 134.78 kB |
| Deferred extension renderer | Included in main | 65.49 kB / 20.49 kB gzip |

Main JavaScript is approximately 7.2% smaller, or 7.9% smaller compressed,
despite the new functionality. This is a startup transfer/parse reduction;
it is not a measured improvement in launch latency or total installed size.
The deferred extension stylesheet adds 4.26 kB (0.96 kB gzip).

## Follow-up improvements implemented

### Daemon-owned readiness

CLI `models check`, TUI model cards and explicit tests, and native desktop model
checks now use `ProbeModel`. Only explicit tests set `network: true`; opening or
refreshing a model list makes no provider request, including for local endpoints.
The negotiated protocol is checked before sending the command. An old daemon
produces a compatibility explanation and unverified state, rather than a false
claim that credentials are invalid. Desktop no longer includes a provider HTTP
transport solely to duplicate daemon probes.

Implementation: [connection.rs](../../crates/council/src/connection.rs),
[commands.rs](../../crates/cli/src/commands.rs),
[tui.rs](../../crates/cli/src/tui.rs),
[model_probe.rs](../../crates/codypendentd/src/model_probe.rs), and
[bridge.rs](../../apps/desktop/src-tauri/src/bridge.rs). Socket tests cover a
legacy daemon and daemon verdicts that differ from the client's credentials.

### Usage completeness without erasing measured spend

Protocol 1.10 adds optional `UsageCoverage` to `RunUsage`, counting requests,
per-dimension measurements, and observed retries without their own usage. The
runtime includes failed requests in coverage while retaining measured subtotals
for budget enforcement. TUI, accessible output, and desktop label partial
coverage; historical events with no coverage are explicitly unknown. Wholly
unmeasured amounts remain absent. Small nonzero costs retain enough decimal
places to avoid appearing as zero.

The additive [0056 migration](../../migrations/0056_run_usage_coverage.sql)
persists coverage alongside totals; historical migration checksums are unchanged.
Every crate embedding the root migrations now watches that directory at build
time, preventing stale compiled migrations after adding a file. A runtime
integration test checks mixed measurements, measured zero, retry, failure,
persistence, and retained subtotal. Rust schemas, golden vectors, and generated
TypeScript include the new field.

### Live desktop extension delivery

[RemoteUiHost.tsx](../../apps/desktop/src/components/RemoteUiHost.tsx) replaces
the unused document-array renderer. The native socket forwards session-bound
Remote UI frames and permits only renderer-originated message kinds on the return
path. A confirmed attachment negotiates before accepting documents. The shared
host store validates snapshots and atomic patches, requests recovery on revision
gaps, and handles disposal. Reconnect remounts an empty host and renegotiates;
ephemeral extension frames do not advance the durable event watermark.

Visible extension identity comes from broker-attested contributions. Document
metadata cannot assert publisher identity. Semantic interactions and mediated
subscriptions/actions return through the native transport. The host offers
recovery controls, retains local hidden-surface decisions for the attachment,
and applies desktop styling to identity, feedback, inputs, and data views.
Tests cover the native socket boundary, stale sessions, broker identity, patch
validation, recovery deduplication, interaction, and reconnect. Rust's omitted
`atomic: true` default is normalized at the SDK boundary.

### Shared bounded diagnostic sanitization

[Rust](../../crates/tui/src/failure.rs) and
[TypeScript](../../apps/desktop/src/failure.ts) now consume one
[contract and adversarial corpus](../../testdata/failure-sanitization.json).
The bounded tokenizer understands quoted JSON keys/values, nested secret values,
HTTP authorization lines, query parameters, and known token prefixes. It retains
adjacent actionable details, strips terminal and bidirectional controls, scans
at most 8,192 characters, and emits at most 2,048. Both clients assert exact
outputs against the same ten fixtures alongside their existing regression cases.
This is a presentation safeguard; raw diagnostic persistence is unchanged and
unlabelled unknown secrets cannot be exhaustively recognized.

### Search generations and provider setup parity

[SessionLibrary.tsx](../../apps/desktop/src/components/SessionLibrary.tsx) now
assigns request generations across searches, continuations, cleanup, and transport
replacement. Old success and error responses cannot enter the current view even
for A → B → A query sequences. Three targeted tests exercise those races.

The CLI, TUI, and desktop share the runtime
[models-file transaction](../../crates/runtime/src/models_file.rs), including
comment preservation, locking, rollback, and removal. ACP adapter handshake and
profile-family replacement also share
[one implementation](../../crates/runtime/src/acp_setup.rs).

Desktop can refresh the agent catalog and explicitly connect an ACP agent in the
selected repository. Selection alone starts nothing. Community risk requires a
checkbox; connection failures preserve retry controls and write no profiles.
Successful handshakes expose discovered models. The native bridge validates the
repository and pinned adapter install before saving profiles. UI tests and a
browser keyboard walkthrough cover consent, missing repositories, focus, errors,
and retry. A live authenticated adapter install was not exercised.

### Release metadata and platform gates

The guide and documentation manifest now target app version 0.14.0. CI and release
quality gates enforce aligned first-party version markers and run native checks
on both macOS and Linux. These are the shipped desktop targets; there is no
Windows desktop release to certify. The guide now distinguishes historical
migration incompatibility from an automatic repair path, which the current
opener does not provide. The
[desktop release smoke checklist](desktop-release-smoke.md) records the required
packaged GUI checks without claiming untested platforms passed.

The older improvement checklist was treated as a lead rather than evidence of
current defects. For example, the executor now has a single
[RunControlRegistry](../../crates/codypendentd/src/executor.rs), and worker
[shutdown](../../sdk/ui/src/worker/runtime.ts) already protects its terminal
state when transport close fails. Reopening those older findings unchanged
would misrepresent current main.

## Continued improvement pass

A follow-up pass focused on completing analytics navigation, protecting session
mutations, and bringing measured usage into readable CLI output.

- **Analytics pagination and recovery:** users can load additional aggregate
  pages. The original time window is retained across cursor requests. A failed
  continuation keeps the loaded measurements and offers retry. New filters clear
  the prior results immediately, and loading never claims that no observations
  exist. Request generations protect both queries and exports across repeated
  filter values, callback replacement, reconnect, and unmount. Retired query
  failures also stay out of the app-wide error banner.
- **Measured subtotals:** each metric names partial bucket coverage when only
  some loaded buckets reported it. Small nonzero cost figures retain six decimal
  places. Missing values remain unavailable. Bucket coverage does not claim that
  every request inside a reported aggregate supplied measurements.
- **Session actions:** per-session pending guards prevent duplicate or conflicting
  lifecycle requests. Rename/delete confirmations stay open on daemon refusal;
  rename drafts survive for retry. Successful mutations refresh search membership
  and retire older search replies and cursors. Replacing the connection clears
  pending UI and ignores stale mutation outcomes. A failed initial search has an
  explicit retry action, and a disconnected library issues no requests.
- **Dialog keyboard behavior:** rename starts on the title, delete starts on
  Cancel, Tab remains inside, Escape restores the trigger, and IME composition
  does not submit or dismiss. While controls are disabled, focus remains on the
  dialog; after refusal it returns to the draft or Cancel. Disabled buttons are
  visually distinct across the desktop, and the rename field fits narrow panels.
- **Readable CLI usage:** measured tokens/cost and partial or unknown coverage
  appear on stderr, leaving stdout suitable for redirected answers. CLI and TUI
  share pure usage/cost formatting, also used for spoken cost amounts and CLI
  workflow costs. The bounded legacy usage drain now supports readable output
  while preserving run ownership and the terminal exit verdict.

New regression tests cover analytics filter/reconnect/page/export races, the
app-level error boundary, partial buckets, session action refusal/retry/focus,
stale mutation replies, CLI stdout/stderr separation, trailing legacy usage,
sibling-run exclusion, and measured zero versus absent amounts. Browser checks
used explicitly simulated local data to verify additional analytics pages,
rename pending focus, retained drafts after refusal, and successful keyboard
retry. Temporary preview files and the local server were removed afterwards.

## Decision and inbox recovery pass

A further pass addressed the desktop path from receiving a request to completing
a decision:

- **Review before deciding:** the inbox no longer offers approval from a summary
  that lacks the full action and risk. Review uses source session provenance to
  attach the owning session, then focuses the matching approval or question.
  An entry without provenance can open an already-loaded matching request;
  otherwise it explains how to find the owning session. Refused attachments keep
  the inbox visible. Leaving and returning during an attach cannot cause a late
  reply to redirect navigation.
- **Truthful decisions:** answer, rejection, and approval promises reach their
  cards. Pending choices share a synchronous guard; neither a duplicate command
  nor the opposite decision can race the first. Command acceptance stays visibly
  distinct from durable resolution. Accepted decisions remain locked across view
  changes. Refusals keep answer and rejection drafts in the mounted card, display
  sanitized feedback, and permit retry.
- **Question reissues and keyboard behavior:** the durable issue sequence resets
  identical reissued questions. Old replies cannot change a newer issue's state.
  Focus stays on the card while its controls are disabled and returns to the
  sending button after refusal, unless the operator has moved elsewhere. Opening
  rejection focuses its optional explanation; Back returns to Reject. Question
  and approval cards use border-box sizing and wrap evidence at narrow widths.
- **Inbox and reconnect races:** each inbox entry serializes its own actions,
  while different entries can proceed independently. Synchronous bridge failures
  and rejected promises produce local errors. Controls disable when disconnected.
  A refresh started before an acknowledgement cannot restore the unread row, and
  retired connections cannot write mutation results into the new inbox. An old
  failed connection's delayed socket-path lookup cannot disconnect its replacement.

This pass adds 18 regression cases across decision recovery, inbox operations,
and daemon/App integration. The complete desktop suite passes with **307 tests
across 44 files**, along with TypeScript and a production build. Browser checks
used simulated commands to verify keyboard submission, refusal with draft and
focus preservation, retry, retained acceptance across navigation, full evidence
at a narrow panel width, and the inbox Review action. Temporary fixtures, browser
tab, and development server were removed. This pass changes desktop TypeScript;
the Rust and SDK results below are retained from the earlier validated passes.

## Artifact transfer and export recovery pass

CLI session and support-bundle exports, TUI session export, and native desktop
Save now share the council crate's
[artifact transfer](../../crates/council/src/artifacts.rs). Each request asks for
at most 1 MiB. The reader checks artifact identity, offset, declared digest,
encoded and decoded chunk bounds, exact final length, and the computed SHA-256.
Truncated replies, empty non-final chunks, and corrupt content fail explicitly.

Exports stream to a private sibling file and sync before atomic replacement.
Interrupted reads, cancellation during transfer, verification failure, and failed
replacement clean up staging and preserve an existing destination. Bare relative
filenames and missing parent directories work. On Unix, saving over a symlink
replaces the link without truncating its target. Save no longer assembles an
entire artifact in memory or inherits the desktop preview reader's 64 MiB cap.
A regression test saves 65 MiB in bounded chunks while confirming that the same
reference is rejected by the capped preview reader before requesting bytes.

Desktop patch previews and save feedback belong to the complete artifact
reference and the active callback. Changing content metadata under the same ID,
replacing the connection, or returning through an A → B → A sequence cannot
display an old result. Equivalent references retain loaded content. Synchronous
guards prevent duplicate requests; keyboard focus remains on the action group
while pending and returns to the trigger after failure unless the user has moved.

The shared module has 12 transfer regression tests. CLI and native socket tests
exercise the real transport adapters, including preservation of an existing file
after same-length corruption. Six new desktop cases cover reference/source races,
duplicate saves, and focus recovery. The current suites pass with **1,218 CLI,
TUI, and council tests; 57 native tests; and 313 desktop tests**. Two earlier native
file helper tests moved into the shared coverage. Clippy passes with warnings
denied, as do the native app feature compile, TypeScript, and production build.
Browser checks with an explicitly simulated transport confirmed that a replaced
preview ignores the old response and a failed Save restores its retry button's
focus. The fixture, browser tab, and development server were removed afterwards.

## Question and queued-draft recovery pass

TUI question events now respect ownership of the visible editor. Adding,
reissuing, or resolving a background question leaves the front draft and
viewport unchanged. Reissuing the front question resets answers, custom text,
rejection feedback, and viewport geometry even if its shape is identical.
Resolving it initializes the next question from its current definition. Unsent
decisions for a replaced or resolved question are removed from the outbox.
Five regression cases cover changed and identical reissues, background queue
activity, stale unsent decisions, and rendering down to a zero-sized terminal.

Desktop queue commands and editor state now live above panel/navigation
unmounts. Save, Send next, and Remove share one synchronous pending guard per
prompt while different prompts remain independent. Refused edits preserve their
draft. Acceptance closes only the exact draft submitted; further keystrokes,
Cancel/Edit, and editing another row cannot be erased by an old response. Prompt
removal or session switching retires its draft, and old connection/attachment
replies cannot modify current error banners. The visible queue still changes
only through daemon events.

Edit focuses its text field, Ctrl/Command-Enter saves, and Escape cancels with
IME composition respected. Pending focus stays on the row and returns after
refusal unless the operator moved elsewhere. Distinct button keys prevent React
from reusing Save as Send next and restoring focus to that different action.
The shared focus-visible rule now overrides inline outline suppression, fixing
the keyboard ring across desktop editors. The guide's question rejection key
description was also corrected against the actual input handler.

Eleven new desktop cases cover draft, pending-command, focus, and transport
generation behavior. **All 324 desktop tests across 45 files pass**, along with
TypeScript and the production build. A browser walkthrough with a simulated
transport verified keyboard submission, pending focus, refusal, retained draft
and feedback after reopening the panel, and the visible keyboard focus ring.
The temporary fixture, tab, and server were removed afterwards.

The CLI, TUI, and council suites pass with **1,223 tests**. An initial run hit
the existing fake voice-recorder fixture's missing-output failure. That case
passed unchanged in isolation, then the complete selected suites passed with
four test threads. No voice behavior or assertion was changed to obtain the pass.

## Opening splash redesign

The terminal welcome screen now uses a linked-agent emblem, a white/cyan/blue/
violet block wordmark, sparse background dots, and corner registration marks.
A highlighted Enter keycap anchors the ready screen. Existing boot ticks drive
the emblem and lettering highlights; the ready gate stays completely still,
and no timer or startup delay was added. All colors come from the active theme.
The rendering lives in a dedicated pure splash module with the existing public
entry point preserved.

Layout reserves space for status, warnings, and Enter before selecting the large
wordmark and emblem. Compact screens drop decoration, warning overflow reports
the exact omitted count, and the workflow ribbon hides when it cannot fit.
Diagnostic text is bounded, sanitized, and truncated by terminal cell width.
The five new regression tests cover layout thresholds under warning pressure,
overflow counts, static readiness in five themes, first-run setup, tiny/empty
terminals, long Unicode diagnostics, and terminal controls.

The full TUI suite passes with **763 tests**, including 742 unit tests and 21
integration tests. CLI/TUI Clippy passes across all targets and features with
warnings denied, as do formatting and the documentation manifest check. Visual
inspection used HTML previews of actual Ratatui test buffers at 100×30, 80×24,
50×16, and 100×18, including light/dark themes and warning overflow. This checks
composition rather than certifying font rendering in every terminal emulator.
The temporary preview fixture, browser tab, and server were removed afterwards.

## Validation and limits

| Check | Result |
| --- | --- |
| Prior full Rust workspace pass, before the continued improvement pass | 4,075 passed; 0 failed; 9 ignored by the suite |
| Prior workspace Clippy, all targets/features, warnings denied | Passed |
| CLI + TUI + council suites, before the splash redesign, all features | 1,223 passed; 0 failed |
| CLI + TUI + council Clippy, before the splash redesign, all targets/features, warnings denied | Passed |
| Current CLI + TUI Clippy, all targets/features, warnings denied | Passed |
| Current TUI suite, including splash layout and VT100 tests | 763 passed; 742 unit tests and 21 integration tests |
| Native desktop Rust tests | 57 passed; shared file helper coverage now lives in council |
| Native desktop Clippy, all targets with app feature, warnings denied | Passed |
| Current native app feature compile | Passed; local binary build and launch passed in an earlier phase |
| Desktop TypeScript, Vitest, production build | Passed; 324 tests across 45 files |
| Web client check (typecheck, tests, build) | Passed; 16 tests |
| Protocol SDK check | Passed; 471 tests |
| UI SDK check | Passed; 86 tests |
| Remote UI SDK check | Passed; 1 test |
| Rust formatting and changed-file whitespace | Passed; fixed-width VT100 snapshot padding excluded from whitespace-only checks |
| Generated schemas, TypeScript, migration immutability, release versions and workflow security | Passed |
| Documentation manifest | Passed; all 222 documents indexed |
| External npm vulnerability audit | Pending user approval; automatic review rejected sending dependency metadata to npm |

Local socket tests initially failed under sandbox restrictions and were rerun
with local socket access; the final workspace run passed. No failure was hidden
by changing the tests to tolerate permission errors.

Browser checks used the real desktop components: disconnected startup,
command-palette navigation, readable approval evidence, full-patch loading, and
refused queued-draft recovery. Patch and approval visual fixtures used explicit
simulated data and executed no commands. The follow-up walkthrough also checked
ACP connection failure/retry, keyboard
focus restoration, broker-delivered extension content, and partial usage labels.
The temporary fixtures and smoke-test processes are removed from the deliverable.
Accessibility checks used browser accessibility state,
keyboard interaction, and the
[Web Interface Guidelines](https://raw.githubusercontent.com/vercel-labs/web-interface-guidelines/main/command.md).

A locally built macOS native window launched with disposable configuration and
socket paths, and its command palette opened by keyboard. This does not certify
a packaged installer or successful daemon onboarding. No live paid model request,
external GitHub mutation, native save-dialog click, or Linux GUI exercise was
performed. The expanded remote CI matrix has not run from this local checkout.
Native file-writing behavior is
tested separately from the dialog; the complete packaged native interaction
still needs a manual platform smoke test. The review covered the main user
flows and relevant shared runtime, protocol, SDK, and transport code, rather
than claiming a line-by-line audit of every repository file.
