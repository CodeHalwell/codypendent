# Coding acceptance — 4 October 2026

The real provider completed the word-report task from the native desktop app. Independent verification reproduced **5 passing tests**, compared with **4 failures and 1 pass** before the run. The acceptance test file was unchanged, and the diff contains exactly `words.py`, `cli.py`, and `README.md`.

## Real provider run

| Field | Value |
| --- | --- |
| Model | `deepseek-v4.1-flash:cloud` |
| Transport | OpenAI-compatible endpoint `http://127.0.0.1:11434/v1` |
| Model selection | `real-code` |
| Session | `01a10815-59ac-7010-aeff-cfb60f3a59f5` |
| Run | `01a10815-59bd-7111-9cff-ea5cc2bb51b8` |
| Final durable state | `Completed` |
| Completed at | `2026-10-04T18:08:01.618762+00:00` |
| Fixture root | `/private/tmp/cody-coding-fivx8sqk` |
| Database | `/private/tmp/cody-coding-fivx8sqk/data/codypendent.db` |
| Run worktree | `/private/tmp/cody-coding-fivx8sqk/codypendent-worktrees/repository/run-ea5cc2bb51b8` |

The objective required case-insensitive word counting, preserved Unicode letters, punctuation handling, `--top N` with count/alphabetical ordering, negative-N rejection, unchanged default JSON mapping output, updated documentation, and an actual test run. The provider used workspace tools to read and write the files. Its approved `python3 -m unittest discover -s tests -v` command succeeded in durable events 51–52. Event 122 records completion and the reported test result.

The generated fixture permits `python3` through its own `data/config/policy.toml`; command approvals remain required. No repository default or global user policy was widened. An unrelated `afplay` request was denied by the existing allowlist, and the run continued to completion.

## Independent verification

After the run completed, a separate process ran:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest discover -s tests -v
```

Result: exit **0**, **5 tests**, **OK**. All five named cases passed: `test_case_and_punctuation`, `test_empty`, `test_negative_top`, `test_top`, and `test_unicode`.

`tests/test_words.py` is byte-for-byte identical to the initial Git version. SHA-256: `3addb91cd472992c3a0d3639eb19fefb2d64d2de86a919b89ae624ece2e4acd4`.

`git diff --name-only` returned:

```text
README.md
cli.py
words.py
```

The changed files remain uncommitted in the isolated run worktree. The daemon retained that worktree and saved its diff instead of deleting changes. This verifies creation and retention of a real coding result; it does not claim a merge into the original checkout.

One uncovered behavior remains in the provider-generated fixture code: `words.py` uses `lower()`, so `word_counts("Straße STRASSE")` returns two separate keys. Full Unicode caseless matching would require `casefold()` and an additional acceptance case. The five fixed acceptance tests do not cover that distinction. The fixture was not edited during independent review.

Local evidence from this run:

- `/tmp/cody-coding-acceptance-tlu7342o/acceptance-tests.txt` — independent test output.
- `/tmp/cody-coding-acceptance-tlu7342o/diff.patch` — independently captured diff.
- `/tmp/cody-coding-acceptance-tlu7342o/summary.json` — changed files, unchanged-test digest, exit code, and Unicode edge probe.
- Diff artifact `01a1081a-142b-7811-8639-828e6185ff31`: `/private/tmp/cody-coding-fivx8sqk/data/artifacts/sha256/8e/8e6a01cbbcb3331fe06aa7ca62f2ff973524845961599d08ef651c1e88289935`.
- Chronicle artifact `01a10819-f648-7073-b6f5-6dba57bd187c`: `/private/tmp/cody-coding-fivx8sqk/data/artifacts/sha256/52/529abb35be6657e6987c550ef27826f93ff1e0c70f6f5b3a01ef224ec2ef9316`.

These are local temporary evidence paths, not published artifacts.

## Runtime replay regression gate

The runtime lane passed five focused replay/wire tests after replacing assistant-style tool-call prose with paired structured tool calls and results. Native Gemini retains a conservative history path because its current transcript does not preserve thought signatures. This gate is separate from the real DeepSeek acceptance run above; it does not establish live Gemini or Anthropic provider acceptance.

## Repeat the real-provider fixture

From the repository root:

```sh
python3 apps/desktop/scripts/coding-fixture.py \
  --model deepseek-v4.1-flash:cloud \
  --base-url http://127.0.0.1:11434/v1
```

The script creates a fresh temporary Git repository, isolated model/desktop/policy configuration, and launch environment exports. It verifies the four-failure/one-pass baseline before printing the objective. It does not contact a provider, generate model responses, copy databases/logs, or store credentials. For an authenticated endpoint, `--api-key-env NAME` stores only the environment variable name; supply its value through the launch environment.

Use the printed `CODYPENDENT_DATA_DIR`, `CODYPENDENT_CONFIG_DIR`, and `CODYPENDENT_SOCKET` values when launching the application. Select the printed repository and `real-code` model, submit the printed objective, and approve the actual commands through the application. Verify the final worktree and unchanged acceptance tests independently.

## Native recovery and source navigation

Actual packaged-app testing with frontend `index-CpDMfKrs.js` quit the desktop client during the real coding run, allowed the daemon to finish, and reopened the application. The full conversation and usage were restored: **58,467 input tokens and 3,517 output tokens**. This exercised recovery from the durable session history after the client process ended.

In the native transcript, the saved diff expanded to `README.md`, `cli.py`, and `words.py`. **Load full patch** retrieved the artifact. Clicking the `cli.py` line **27** gutter opened VS Code through the contextual session URI. The URI identified the base repository, originating run checkout, session, and daemon socket. Only the disposable run checkout created for this test was trusted. VS Code visibly showed `cli.py` at **Ln 27, Col 1** under the retained run path, with the Codypendent panel attached to the **Completed** session and its conversation history.

Evidence: `/tmp/cody-coding-acceptance-tlu7342o/native-review.json`. It records the retained source file `/private/tmp/cody-coding-fivx8sqk/codypendent-worktrees/repository/run-ea5cc2bb51b8/cli.py`, the original coding session, full-patch load, and exact editor position. The same native check typed `--top N "quoted"` into the composer and preserved the literal characters.

Subsequent final debug-bundle lifecycle and offline recovery checks used frontend `index-yzwxz8Al.js`. Offline recovery controls retained the same accessibility button identities across automatic retries. Explicit **Connect workspace** started the bundled daemon and reconnected. The patch/editor/restoration evidence above remains attributed to the earlier `index-CpDMfKrs.js` bundle.

## Native pause, resume and cancellation

For run `01a1083d-ba8b-7840-96d6-3f8711555df8`, the native cancellation dialog showed the actual objective and elapsed time, first **10 seconds** and later **2 minutes 2 seconds**. This verifies the launch-event handoff correction in the final debug bundle.

The run paused while `git --version` awaited approval. Approval was then granted, but the run remained **Paused** and emitted **no `ToolStarted` event** during the next **43 seconds**. Cancellation changed the run to **Cancelled**, still with **zero tools started**. The durable export `/tmp/cody-coding-acceptance-tlu7342o/lifecycle-paused-cancel.jsonl` records `Paused` at `18:48:22.006649+00:00`, approval at `18:48:29.140939+00:00`, and `Cancelled` at `18:49:12.406327+00:00`.

The first explicit native resume check **failed** for run `01a10843-53fe-72a2-8660-4a486505b3e4` in session `01a10843-5390-7a62-8c99-8cb8200c82a5`: the UI reported `Some(Running) -> Running` as a refused stale runtime transition. Approval had left the run paused without a tool start; clicking **Resume** exposed a duplicate transition after the daemon had already persisted `Running`. The runtime correction's focused guarded-transition regression subsequently **passed**. The original failure is preserved in `/tmp/cody-coding-acceptance-tlu7342o/lifecycle-resume-before-transition-fix.jsonl`.

The corrected native debug package then **passed explicit resume** using the real Kimi provider selection `real-alternative`, session `01a1084f-3431-7fd0-8f7e-2f8c3111c7ed`, and run `01a1084f-3445-7ef3-bd79-4a86be440689`. The fresh debug CLI built in **48.36 seconds** and was staged while the bundle was stopped; the frontend remained `index-yzwxz8Al.js`. Bundled-daemon startup and reconnect succeeded, with debug runtime-seal validation taking **13.174 seconds** under load.

| Durable event | Sequence | UTC time |
| --- | ---: | --- |
| Paused | 13 | `19:06:30.195369` |
| Approval resolved | 14 | `19:06:34.957248` |
| Running after explicit Resume | 15 | `19:06:45.338739` |
| Tool started | 16 | `19:06:45.346839` |
| Completed | 28 | `19:06:47.014263` |

A database check after approval confirmed **Paused** with no tool started. Only the explicit **Resume** action released execution. The export contains exactly **one `ToolStarted`** and its successful result at sequence 17. The native UI displayed `git version 2.54.0 (Apple Git-157)`, exit **0**, and usage of **10,938 input / 152 output tokens**. Evidence: `/tmp/cody-coding-acceptance-tlu7342o/lifecycle-resume-after-transition-fix.jsonl`. This passes the corrected **debug native** lifecycle.

The separate completed run `01a1083f-f825-7a61-bb74-f9a1008cb8ae` was approved during a handoff between verification steps and is not pause/resume evidence.

## Desktop review browser QA

The separate browser fixture imports the production `Transcript` and `PatchReview` components from `apps/desktop/test/browser-review/main.tsx`. Its patch data and editor callback are synthetic display fixtures. It makes no daemon, model, or native editor calls and is not evidence of native gutter-to-IDE integration.

Actual Chrome interaction verified:

- Touched-file selection and previous/next navigation between hunks and files.
- Keyboard Enter from the last text hunk into a rename-only file. A discovered focus loss was fixed by retaining one navigation control row, then retested in the browser.
- Source-line callback `src/words.ts:7` with `original-fixture-run`, plus displayed errors and restored focus after a deleted-file open failure.
- A 450-line addition paged through 1–200, 201–400, and 401–450. The final accessibility table contained 51 rows including its header.
- An unsafe `../outside.ts` path remained disabled, with the raw patch available for reading.
- Light/dark rendering. A discovered wrapping defect in the Before/After/change headers was fixed with non-wrapping header text and visually checked again.

All **23 focused patch-review tests** passed after the navigation fix. The production TypeScript/Vite build passed. Final embedding was handed to the native build lane after its repository-menu cleanup.

A later native offline check exposed recovery controls disappearing during automatic connection retries. A presentation-only connection state now keeps the banner, navigation status, toolbar, and empty transcript offline after failure until a connection succeeds. Initial connection and explicit service startup still show progress. All **11 focused initial-connection and daemon-launch tests** passed, including button identity/focus through retries, initial connection, explicit startup, and a fresh connection cycle. Transport state, security checks, and retry timing were unchanged. The native recovery check above subsequently verified stable controls and explicit bundled-daemon startup.

Native lifecycle QA then exposed a cancellation dialog with unknown objective and elapsed time for run `01a1082e-9c2e-7711-878c-563fc2b6eb7c` in session `01a1082e-9c14-7320-aea3-ee034d50be8d`. Its `RunStarted` event was present in the database at `2026-10-04T18:30:34.798646+00:00`. The frontend had discarded early events for the newly created session while waiting for the launch reply. It now buffers those frames, binds only the accepted session's frames, and preserves lifecycle transitions that preceded the reply. The dialog continues to use the durable event's objective and timestamp. **29 focused launch, App, and attachment-boundary tests passed**, including the visible dialog fields, rejected/disconnected launches, unrelated sessions, and early completion. The native cancellation check above subsequently verified both fields.

Screenshot: `/tmp/cody-review-mixed-dark.jpg` shows the final header layout and exact-line callback. `/tmp/cody-review-large-page.jpg` captures the final 50-line page before the small header wrapping correction.

To build and serve only this display fixture, leaving the application's `dist` untouched:

```sh
cd apps/desktop
export CODYPENDENT_REVIEW_DIST="$(mktemp -d /tmp/cody-review-browser-XXXXXX)"
node --input-type=module <<'JS'
import { build } from "vite";
await build({
  configFile: false,
  root: "test/browser-review",
  build: { outDir: process.env.CODYPENDENT_REVIEW_DIST, emptyOutDir: true, target: "es2021" },
});
JS
python3 -m http.server 51289 --bind 127.0.0.1 --directory "$CODYPENDENT_REVIEW_DIST"
```

Open `http://127.0.0.1:51289`, expand the patch, and use the Scenario and Appearance controls. The server used for this verification was stopped afterwards.

## TUI patch navigation

The actual CLI passed **14 PTY checks**, exit **0**, against the real coding session. These covered the splash/composer, Ctrl-F exact file-list search, selecting its saved patch, **Alt-Shift-N/P** moving between files, **Alt-N/P** moving between hunks with backward navigation stopping at the beginning, **Alt-Enter** returning to the full preview, the workspace panel, and clean detach.

```sh
uv run crates/cli/tests/pty_smoke.py \
  --binary target/debug/codypendent \
  --data-dir /private/tmp/cody-coding-fivx8sqk/data \
  --socket /private/tmp/cody-coding-fivx8sqk/daemon.sock \
  --repository /private/tmp/cody-coding-fivx8sqk/repository \
  --output /tmp/codypendent-tui-patch-navigation-evidence \
  --review-patch 'README.md cli.py words.py'
```

Evidence in `/tmp/codypendent-tui-patch-navigation-evidence` includes `result.json`, `terminal.raw`, and text screens. Navigation operates on the inline diff preview; oversized full artifacts remain separate.

The subsequent TUI-to-IDE check passed **18 PTY checks**, exit **0**, in `/tmp/codypendent-tui-ide-app-handoff-evidence`. It selected `cli.py` at **Hunk 2/3**, then **Alt-O** displayed `Opened cli.py:1 in VS Code · same session` and preserved the unsent draft `handoff draft - do not send`. After the one-time editor URI/session prompts were accepted, actual VS Code displayed **cli.py — run-ea5cc2bb51b8**, **Ln 1 Col 1**, with Codypendent attached to session `01a10815-59ac-7010-aeff-cfb60f3a59f5` and the run **Completed**. No objective was submitted or source file edited. Repeat the preceding PTY command with `--open-reviewed-source cli.py` and this separate output directory. `result.json`, `06-source-hunk.txt`, `06-editor-handoff.txt`, and `vscode-handoff-result.json` preserve the evidence. Two focused TUI and two CLI handoff tests passed, including Git's tab-terminated path header and nested repository anchoring.

## Bundled Node compatibility

The [official release index](https://nodejs.org/dist/index.json) identified **Node 22.23.3**, dated **2026-09-23**, as the latest 22.x release. The Apple Silicon archive matched its [official SHA-256 manifest](https://nodejs.org/dist/v22.23.3/SHASUMS256.txt): `23b25245dcfb9af7262f8ff142e9e2e0af025368117329e7a7458a51e5922f53`. A fresh runtime was prepared at `/tmp/cody-node22-update-1hqlo6on/node-runtime`, sealed with `apple-release.py`, and checked without altering the original helper runtime. The binary reports arm64 and a macOS 11.0 deployment minimum.

Using that exact Node binary, **19 focused SDK worker tests passed across four files**. The existing runtime's npm and Corepack modules also ran. A compiled React worker then completed framed handshake, ready, rendered snapshot, ping/pong, and disposal with production Node permission/heap/prototype/addon flags, exit **0**, and no stderr. Evidence: `/tmp/cody-node22-update-1hqlo6on/compatibility.json`.

That first successful worker check invoked Node directly. The existing native worker launcher separately refused `RLIMIT_DATA` on macOS 27.0.1 before Node started. After its correction, `/tmp/cody-launcher-check.92ktZU/release/codypendent-ui-worker-launcher` ran the same compiled React fixture with **128 MiB** requested memory, **30 CPU seconds**, Node 22.23.3, and the same production Node flags. Framed handshake, ready, rendered snapshot, ping/pong, and disposal all passed, exit **0**, with no stderr. Evidence and the exact command are in `/tmp/cody-node22-update-1hqlo6on/launcher-compatibility.json`.

The same check subsequently passed against the final `target/release/codypendent-ui-worker-launcher`, again exit **0** and no stderr. Its SHA-256 is `72d83e6ac15f1a9bdcc0cc414b017b34b109ed13f4bc9e8612c445ca7823ab4c`; distinct final-binary evidence is in `/tmp/cody-node22-update-1hqlo6on/launcher-final-compatibility.json`. Complete packaged-app verification remains separate from these worker checks.

## Debug native GUI idle sample

With the original coding session restored and its patch card closed, the debug app was sampled for **30.7467 seconds**, yielding **17 observations**. The explicitly identified process set was main **31644**, WebKit GPU **31999**, WebContent **32002**, and Networking **32004**; the helpers appeared within two seconds of this app launch after the old processes had exited. No unrelated WebKit processes were included.

`apps/desktop/scripts/measure-gui-idle.py` measured cumulative `ps` CPU-time deltas and each selected process's RSS. All four CPU counters were unchanged at `ps` resolution; this is **zero observed CPU-time delta**, not a claim of no work at finer resolution. Summed RSS was **203.094 MiB** at the end, **212.171 MiB** across sample means, and **244.141 MiB** at its peak. Summed RSS includes shared pages counted in multiple processes.

Evidence: `/tmp/cody-gui-idle-debug-final.json`, recorded from `2026-10-04T19:08:37Z` to `19:09:10Z`. The host had **64 GiB RAM** and **18 logical CPUs**; 1/5/15-minute load averages changed from **64.31 / 78.58 / 73.50** to **75.14 / 79.61 / 74.04**. Raw memory counters, per-PID samples, actual sampling duration, and process identities are preserved in the JSON. This is a debug observation under heavy host load and memory reclamation, not a controlled before/after comparison. The later optimized GUI sample is recorded below; the samples are not a controlled before/after comparison.

## Optimized native GUI verification and idle sample

The release GUI with frontend `index-BwrVq75T.js` and a **46,753,872-byte main executable** restored the real completed coding session and displayed **58,467 input / 3,517 output tokens**. Expanding the saved patch and choosing **Load full patch** displayed all three changed files: `README.md` **+32/−1**, `cli.py` **+39/−5**, and `words.py` **+36/−1**. Selecting `words.py` rendered its complete **40-line hunk**. The app was then quit and main PID **22104** was confirmed gone. This GUI check used the existing corrected debug daemon **51984**; the large-fixture native check and final bundled release-daemon checks are separate.

The read-only idle sample `/tmp/cody-gui-idle-release.json` is **valid**, covering **30.343918 seconds** and **17 observations** from `2026-10-04T19:41:57Z` to `19:42:29Z`. Its explicitly identified processes were main **22104**, WebKit GPU **22656**, WebContent **22657**, and Networking **22671**. The daemon was excluded. Each process and their total recorded **0.0 seconds cumulative CPU-time delta at `ps` resolution**.

Summed RSS was **159,842,304 bytes (152.4375 MiB)** at the end, **185,349,300.706 bytes (176.763 MiB)** across sample means, and **202,883,072 bytes (193.484375 MiB)** at the peak. These sums count shared pages more than once. The macOS 27.0.1 host had **64 GiB RAM** and **18 logical CPUs**; 1/5/15-minute load averages changed from **53.43 / 53.66 / 57.24** to **58.10 / 55.21 / 57.71**. The JSON retains per-PID measurements and memory counters. This is an optimized GUI observation under host load and memory reclamation, not a controlled comparison with the debug sample.

## Synthetic frontend review benchmark

Measured the production-built components in Chrome on **macOS 27.0.1 (26A434), Apple M5 Max, 64 GiB RAM**, with Chrome reporting version **154.0.0.0**, 18 logical processors, a 1488×945 viewport, and device pixel ratio 2. The native Rust build was running concurrently. These measurements are **browser component timings, not native application latency or a real-provider benchmark**.

The workload contains **10,000 added lines across 10 files and 20 hunks**, totaling **436,820 UTF-8 bytes**. Each bounded series contains one initial cycle, two warm-up cycles, and seven measured cycles. Three series were run; the last separated synchronous React commit from forced style/layout. Timings do not include browser paint, GPU work, daemon IPC, or startup. The isolated parser timing excludes rendering; first expansion includes parsing and mounting the actual production transcript review.

Final series, recorded at `2026-10-04T18:30:50.182Z`:

| Operation | Median ms | Min–max ms | Commit median ms | Layout median ms |
| --- | ---: | ---: | ---: | ---: |
| Parse complete patch | 2.4 | 0.9–45.3 | — | — |
| Mount closed card | 1.7 | 0.9–18.1 | 0.7 | 1.0 |
| First expansion | 32.2 | 18.3–111.0 | 12.9 | 15.7 |
| Next 200-line page | 34.8 | 17.0–305.8 | 10.4 | 15.7 |
| Next hunk | 33.9 | 15.6–63.5 | 10.8 | 19.4 |
| Select last file | 16.6 | 9.2–72.5 | 5.6 | 10.5 |

Commit and layout medians are computed independently, so they need not sum to the total median. Across all three series, parser medians ranged from 1.4–2.4 ms; first-expansion medians from 29.5–42.8 ms. Outliers affected parsing, commit, and layout under the concurrent build load. No production hotpath refactor was made from this sample.

Every measured cycle asserted **zero mounted review subtrees while the card was initially closed**, then **one table with exactly 200 body rows** after expansion, page change, hunk change, and file change. The visible first code line changed to the requested page, hunk, and file. Source inspection confirms `PatchCard` defers `PatchContent` until its first expansion, and `PatchReview` memoizes parsing per source while slicing the selected hunk to one 200-row page. Other hunks/files are retained as parsed data, not additional rendered tables.

Local evidence:

- `/tmp/cody-review-benchmark-1.json` and `/tmp/cody-review-benchmark-2.json` — first two seven-sample series. Their `coldMs` field means the excluded first cycle of each series; the second series did not restart Chrome.
- `/tmp/cody-review-benchmark-profile.json` — final series, including all individual commit/layout samples and DOM assertions.
- `/tmp/cody-review-benchmark-profile.jpg` — captured browser results.

Repeat from the repository root with the dedicated display-only fixture:

```sh
cd apps/desktop
export CODYPENDENT_BENCH_DIST="$(mktemp -d /tmp/cody-review-bench-XXXXXX)"
node --input-type=module <<'JS'
import { build } from "vite";
await build({
  configFile: false,
  root: "test/browser-review-bench",
  build: { outDir: process.env.CODYPENDENT_BENCH_DIST, emptyOutDir: true, target: "es2021" },
});
JS
python3 -m http.server 51290 --bind 127.0.0.1 --directory "$CODYPENDENT_BENCH_DIST"
```

Open `http://127.0.0.1:51290` and click **Run bounded benchmark** once per desired series. This fixture imports the production components and never contacts a model, daemon, or editor. It builds into a temporary directory without changing the application's production `dist`.

## Synthetic 25,000-note history benchmark

Inspection of `/tmp/cody-workspace-perf-4uww00q3` showed that its 25,000 ordinary `NoteAppended` events become visible system messages. They are not the context/memory notes folded into backstage cards. Previously, each replayed append copied the growing transcript and the component mounted every message. Bulk replay now appends to a privately owned transcript until the completed projection is published; live events retain immutable arrays. The ordinary append-heavy replay no longer performs quadratic transcript copying. This is not a claim that all event types are linear: backstage merging and decision lookup have separate costs.

The transcript now mounts at most **200 message rows**, with **First / Older / Newer / Latest** controls. Older pages use stable item anchors while messages arrive or earlier decisions disappear. Returning to the latest bottom resumes following the stream. Inbox links reveal and focus their message's page; confirmed session changes reset the page independently of the composer.

The isolated production build at `apps/desktop/test/browser-history` uses the same synthetic note count and message format, the actual reducer, and the actual transcript/patch components. It does not connect to the fixture daemon, native app, provider, or editor. In Chrome **154.0.0.0**, macOS **27.0.1**, Apple **M5 Max / 64 GiB**, 18 logical processors, 1488×945 viewport, the series recorded at `2026-10-04T19:28:33.588Z` excluded its initial cycle and two warm-ups, then measured seven cycles:

| Operation | Median ms | Min–max ms |
| --- | ---: | ---: |
| Reconstruct 25,000 ordinary notes | 9.1 | 6.8–10.8 |
| Fresh 200-row React mount and forced layout | 7.1 | 5.6–13.8 |

Every cycle retained **25,000 transcript items** but mounted **200 rows**. The excluded initial cycle measured 16.2 ms replay and 42.0 ms mount/layout. These timings exclude IPC, WebKit, browser paint/GPU work, and native startup; the optimized native large-history check remains separate. The native attachment still reads all durable events before publishing the history, so a 100-event IPC page measurement does not establish full-history UI latency.

Actual browser keyboard interaction verified both history ends and intermediate pages, a stable older page during live appends, and the unchanged draft `keep --top N draft`. An archived approval focused the first page and its synthetic callback succeeded. An archived patch stayed expanded after a live note; its line-2 callback retained `src/example.ts:2` and `original-run`. **54 focused tests** passed across large history, reconnect replay, decision recovery, attachment boundaries, patch review and backstage folding. TypeScript and the production Vite build passed: `index-BwrVq75T.js` **449.03 kB / 128.73 kB gzip**, `index-YbBFzul5.css` **14.02 kB / 3.33 kB gzip**.

The subsequent native release check of `index-BwrVq75T.js` successfully started its bundled daemon with a minimal `PATH` and attached the actual large fixture. The UI reported **25,004 transcript items**: 25,000 notes plus four original messages. **First** showed the initial model reply and note `00000` in messages **1–200**; **Newer** showed messages **201–400**, beginning with note `00196`; **Latest** showed messages **24,805–25,004**, with screenshot-visible notes **24983–24999** at the bottom. The draft remained visible after choosing First and was then cleared. Library search for `00900` returned two results, the matching session title and the session containing history note `00900`; opening session `00900` showed its empty **Ready** state. Native accessibility coalesced adjacent notes, so the independent 200-row DOM count comes from the browser check, not the native accessibility tree. The exact historical native record is `/tmp/cody-native-large-history-review.json`.

That native check exposed one initial-scroll defect: the first attachment showed the top of the latest page, while explicitly choosing Latest reached its bottom. A layout effect now positions first/bulk history before paint; normal live tokens retain frame-coalesced scrolling. Final code review also found that resolving the first item anchoring an older page jumped to latest. The page now retains the saved position, clamps to shortened history, and anchors the surviving message. **20 focused history/decision tests passed** after both corrections, including delayed animation frames, removed anchors, shortened history, live following and inbox focus. TypeScript/Vite passed again, producing intermediate `index-D3748xJW.js` **449.38 kB / 128.85 kB gzip**; CSS is unchanged. Its installed native check passed initial scrolling, visibly showing notes **24983–24999**. The browser timings above preceded these scrolling corrections.

That intermediate native check also reproduced a **Focus Composer failure** through both Cmd-L and the Workspace menu, while direct textarea clicks worked. Composer focus now occurs immediately in the post-commit passive effect after palette/shortcut cleanup, without waiting for an animation frame. **26 focused workspace/focus/palette tests passed**, including held frames, Sessions/Models/palette/shortcuts origins, repeated focus requests, preserved drafts and blocking confirmations. The final TypeScript/Vite build passed: `index-CsoSMppT.js` **449.31 kB / 128.82 kB gzip**, with unchanged `index-YbBFzul5.css` **14.02 kB / 3.33 kB gzip**.

Evidence: `/tmp/cody-history-browser-benchmark.json` and `/tmp/cody-history-browser-review.jpg`. Rebuild this display-only harness without changing the application bundle:

```sh
cd apps/desktop
export CODYPENDENT_HISTORY_DIST="$(mktemp -d /tmp/cody-history-browser-XXXXXX)"
node --input-type=module <<'JS'
import { build } from "vite";
await build({
  configFile: false,
  root: "test/browser-history",
  build: { outDir: process.env.CODYPENDENT_HISTORY_DIST, emptyOutDir: true },
});
JS
python3 -m http.server 56899 --bind 127.0.0.1 --directory "$CODYPENDENT_HISTORY_DIST"
```

Open `http://127.0.0.1:56899` and choose **Run 7 measurements**. The verification server and disposable browser tab were stopped afterwards.

## Earlier installed native candidate — protocol 1.10

The final app, frontend **`index-CsoSMppT.js`**, was copied from the **read-only mounted DMG** into a disposable Applications directory. The DMG was unmounted before this native verification. GUI SHA-256: `1f1dae5cbb9b4e11a8a3355426df352c5af69b1fe45271a71c1adfa9dab1387e`.

With **`PATH=/usr/bin:/bin:/usr/sbin:/sbin`**, native **Start** successfully launched its bundled daemon and connected using protocol **1.10** against `/tmp/cody-workspace-perf-4uww00q3`. Its **25,004-item** history initially showed the bottom of the latest page: screenshot-visible notes **24983–24999**. Initial scrolling **passed**.

Actual installed-app interaction then passed **Cmd-L → paste without clicking the composer**. Navigating to Session Library, choosing native **Workspace → Focus Composer**, and pasting also returned to the session and retained the original draft. **First 1–200 → Newer 201–400 → Latest 24,805–25,004** preserved the draft across every page. It was cleared without submission; no provider request or new objective was created. Native accessibility established the displayed range, while the exact 200 mounted DOM rows remain independently verified by the browser fixture.

GUI **47550** was quit and bundled daemon **52411** stopped; both original processes were verified absent. Evidence: `target/apple-release-candidate-2026-10-04/evidence/native-installed-app-review.json`. This final installed package passes the recorded startup, history, focus and draft checks on the development Mac. Apple certificate-backed signing/notarization and clean-Mac Gatekeeper verification remain outstanding.


## ACP transcript and conversation repairs

The installed app's actual session `01a108f2-b32c-7ef0-b7cc-87b4217b6a44` recorded 20 tool starts and 20 successful completions. Starts used generic `Terminal` / `Read File` titles; completions used ACP invocation IDs. No cards matched their completions. The flex transcript column also shrank overflow-hidden tool cards to their borders. The first `Hello` and its reply were saved in another session because every Send created a session.

Transcript children now retain their natural height. ACP calls use stable invocation identities, with bounded command/path labels refined through protocol 1.11's additive `ToolUpdated` event. Desktop, TUI, CLI/ACP and VS Code consume these details. Out-of-order calls with the same title settle independently within their run. Old unmatched calls settle as `finished · result unavailable`; no historical per-call result or command is guessed. Existing approval records can retain commands, but the old generic start records cannot be reliably paired to those IDs.

Send now continues the confirmed attached session. The header, command palette and native File menu offer New Chat (Cmd/Ctrl-N). Opening a new chat retains the draft and is blocked while a run or launch is active. An immediate completion received before the StartRun reply no longer resurrects a completed run.

Focused validation passed: 23 ACP mapping tests, two schema-helper tests, an actual initialise/session/prompt ACP wire test, 20 TUI tool/render tests, 10 desktop tool outcome tests, 25 transcript/history/focus tests and 51 continuity/launch/reconnect/App/chrome tests. Native socket transport verified two objectives use one session, with only one CreateSession/AttachSession, and refuses a stale session ID. Rust golden vectors, 436 SDK vectors, 160 extension vectors, generated-type drift and SDK/extension typechecks passed. These are focused checks; no full workspace suite was run for this repair.

Actual browser checks verified readable tool cards, command/path headers, preserved scroll position during new output, both messages and replies in one chat, New Chat draft retention and independent new-session creation. Final frontend: `index-Y8u8TiHq.js` 453.32 kB / 130.01 kB gzip; `index-C5BjmRy-.css` 14.51 kB / 3.38 kB gzip. Browser fixtures and metrics are synthetic. Native packaged verification is recorded below after installation.


### Updated installed native app — 2026-10-05

The refreshed complete app was installed into `/Applications/Codypendent.app`; its previous bundle is retained at `target/app-update-backups/20261004-235353/Codypendent.app`. A SQLite snapshot of the user's database was saved before the update. The idle daemon was restarted with the bundled CLI and the actual workspace connected using protocol **1.11**. Installed GUI SHA-256: `bb773172113e023210be3268b682be3c437bb12602dfdd38ff96f44f6b324e02`; CLI SHA-256: `d0790d558dd446db59ff044c5c7876b37af8c5649440f1b70dc26345a58bcd8e`.

Actual native accessibility and screenshots showed all **20** historical tool cards with readable natural-height headers and neutral **finished · result unavailable** states. None remained running. Opening **Hello** showed both the first message and its reply; they had been stored in a separate chat. No new objective was submitted in the user's workspace.

An isolated native instance of the same installed app used a loopback-only provider, its own data/config/socket paths and minimal `PATH`. Its transcript replayed actual wire-mapped ACP events into a synthetic fixture. The native screenshot showed **git status --short / Terminal / error**, its failure detail, and **src/main.rs / Read File / success**. Both initial fixture objectives and replies remained in the chat.

Two actual native **Send** actions added two more objectives and streamed replies to that same session. Read-only SQLite inspection confirmed all four runs share one session ID and are Completed. **Cmd-N** cleared the selected chat and preserved the typed draft; sending that draft created a different session. Reopening the original chat restored its four objectives, replies and tool outcomes. The isolated GUI was quit and its daemon stopped; the normal workspace was reopened on the affected saved conversation, connected to protocol **1.11**, with an empty composer. User run states remained **five Completed / one Failed**.

Evidence: `target/apple-release-candidate-2026-10-04/evidence/native-transcript-continuity.json`. The refreshed unsigned DMG and archives passed complete-bundle verification and all **8 installer checks**. This native pass covers the repaired transcript, labels, lifecycle and chat continuity on the development Mac; signing/notarization remains unavailable until Apple credentials exist.


### Accepted Send returns to the latest turn

The installed Y8u8TiHq native check found that Send preserved an older reading position even after adding the operator's own message. Accepted local Send and Retry now explicitly select Latest and place new rows synchronously. New rows after acceptance also position before paint; token updates remain frame-coalesced. A refused Send retains the draft and reading position, and unsolicited output preserves manual history reading.

**36 focused continuity/history/App/focus tests passed**, including animation frames held during accepted sends and later row arrivals. TypeScript and production Vite passed: `index-ChDg7rXI.js` **453.67 kB / 130.09 kB gzip**, with unchanged `index-C5BjmRy-.css` **14.51 kB / 3.38 kB gzip**. In the actual optimised browser fixture with animation frames held, background output retained window **0:200** at scrollTop **0**; accepted Send selected **455:655** at the bottom (**-0.5 px** rounding distance); refused Send retained window **0:200** and the draft. These are synthetic browser measurements. The final installed native result is recorded separately below.


The final installed native app, GUI SHA-256 `4a0d26c4bb7fcf880c67c0090fcc9831fb7f4fe60186daa0353cfe2d7f6358da`, embeds **index-ChDg7rXI.js**. In the actual native app the operator scrolled to the first fixture message, typed a new objective, and pressed Enter. Without another manual scroll, the latest objective and then its streamed reply became visible at the bottom. Completion restored idle controls. SQLite confirmed the new Completed run retained the original session ID. Command/path tool labels and separate error/success outcomes remained readable. Cmd-N retained a new draft; reopening the original chat restored all five objectives. The test draft was cleared, the isolated GUI quit and its daemon/provider stopped. The normal app was reopened, Hello and its reply verified again, and the affected original conversation left open with readable historical cards and an empty composer. User run states remained five Completed / one Failed. Final evidence: `target/apple-release-candidate-2026-10-04/evidence/native-final-send-scroll.json`.

The final DMG/desktop archive passed filesystem, read-only mounted bundle and Node-seal verification. The eight installer checks remain attributed to the unchanged CLI and preceding Y8u8TiHq GUI; they were not rerun for the frontend-only ChDg7rXI refresh. Final DMG SHA-256: `5d64a925e17db26b4c705079c3ae8637c7b3be7c0fc8523d79761c5543dbe70f`. The preceding complete app is retained at `target/app-update-backups/20261005-001933/Codypendent.app`.
