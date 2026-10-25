# Compact tool activity and reusable approvals

Completed tool calls now sit in a closed Activity row for each contiguous run of work. Its count includes calls, with separate failed, blocked, unavailable and running counts. The current running action stays visible in the summary. Expanding the history restores the existing ordered action cards, arguments and results inside a bounded, searchable scrolling area. Closing it unmounts the action cards. The durable event log and stored outputs are unchanged.

Ordinary stored result artifacts can be grouped; interactive generated results and patch cards remain visible. Approvals, questions, assistant replies, run failures and unrelated warnings split groups. Policy-denied tool attempts remain in their own run's activity history with a visible blocked count. Stable source-group identity preserves an expanded search when the 200-item transcript window advances.

Approval cards now offer Always allow alongside Approve and Reject. The existing daemon scopes are used atomically: the exact action for this run; a daemon-provided command pattern for this run; or that command pattern for future runs in this repository. The menu states each scope explicitly. Reconnect snapshots now preserve the same reusable pattern. Unsupported broad patterns are never inferred by the UI. Existing controller, expiry, duplicate and refusal handling remains in force.

Persistent rules now derive repository identity from the approving run's original command within the same approval transaction, preventing a later command in a concurrent session from redirecting a rule into another repository.

## Focused checks

- Activity tests cover 50 collapsed calls, searching, live status/error changes, visible approval focus, ordinary versus interactive artifacts, separate runs, blocked attempts and the sliding 200-item window.
- The final activity/history/decision/presentation run passed 45 tests. The preceding transcript label and AG-UI cases also passed.
- Approval UI tests passed 31 cases; the final consent focus regression passed eight cases.
- Broker approval tests passed 17 cases; run provenance passed two; SDK approval-scope vectors passed four.
- Native forwarding verified Once, Run, Pattern and Repository scopes and rejected invalid values.
- Browser interaction verified 50 calls collapse, search finds one action, and the synthetic repository-scope choice reaches the callback as Repository.
- TypeScript, frontend, CLI and native desktop builds passed. Self-contained installed bundle verification passed.

Native acceptance and executable hashes are under `target/tool-activity-2026-10-19/`.

The final installed app restored Daniel's actual Tell me about this code base chat. Its 62 previous calls and policy denials now form one closed Activity row showing six failed calls and five blocked attempts. Native expansion exposed the original ordered history; searching primitives.py found two matching actions. The generated interactive answer remained outside the archive. The app was left connected on the original chat with history folded. No real permission rule was granted during UI acceptance; scope selection used the labelled synthetic browser fixture, with actual forwarding and persistence covered by the focused native/daemon tests.

## Transcript polish

The saved Hello conversation now prints short progress headings as regular muted text, combines same-run progress and ordinary actions into work phases, and keeps one closed Activity archive beneath each phase. Approvals, questions, interactive results, patches, user messages and assistant answers remain boundaries. Decorative Codypendent speaker headers and the context-carried system line no longer interrupt the answer. Stable group identity retains disclosure state when the 200-item window advances.

Local Markdown citations now show readable labels instead of full absolute worktree paths. Source buttons forward the owning historical run and line to the native editor handoff. Canonical absolute files are accepted only inside that run's authorised checkout; parent traversal, sibling checkouts and escaping symlinks remain rejected. Web destinations remain visible and copyable. A removed historical checkout produces a short unavailable message.

The final focused transcript/activity/Markdown run passed 31 tests in 2.08 seconds; six native source-boundary tests passed. Frontend and native builds and the installed self-contained bundle check passed. The actual installed Hello chat displayed Activity groups of 11 and 12 calls, quiet inline progress, intact generated content and short source references. Native expansion exposed all 11 archived actions and folded again. Its agent.py citation correctly reported that the original run checkout was no longer available; opening that deleted checkout did not succeed. The daemon stayed running throughout. Screenshots, accessibility evidence and the installed executable hash are in `target/transcript-polish-2026-10-19/`.

## Failure reasons and search recovery

ACP failures now preserve bounded text, structured error/stderr fields and exit status or signal, including diagnostics streamed before an otherwise empty completion. The per-prompt tool-call cache retains those earlier updates by invocation ID. Raw MCP error content is projected into the actual human-readable error; arbitrary metadata and credentials are excluded. Native failed tool events now record the real diagnostic rather than only a code such as `tool.error`. Long commands/stdout cannot push the original failure status and stderr out of the retained message; JSON error fields are projected separately before composition.

Host-owned `workspace.search` waits for the actual process exit, captures bounded stderr and retries once for a timeout, SIGKILL or exit 137 within one shared deadline. No match, invalid regex, missing file, access refusal, cancellation and spawn failures are not retried. An exhausted retry explicitly reports two total attempts, and agent guidance counts that against the same budget. External agent built-in shell tools remain owned by that agent; Codypendent exposes and recommends its host-owned search rather than pretending notifications can replay an external call.

Thirteen focused search tests and thirteen ACP failure/history tests passed, along with the live streamed failure test and the native failed-journal and long-output/JSON diagnostic regressions. The installed Codex connection then demonstrated one expected invalid-pattern failure followed by a valid `README.md:3` match. Both persisted native/ACP failures and the real desktop archive displayed `unclosed character class` with exit status 2; no raw JSON was visible. Evidence: `target/agent-features-2026-10-19/`.
