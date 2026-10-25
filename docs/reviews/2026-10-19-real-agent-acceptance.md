# Real agent and model acceptance

Date: 19 October 2026. These checks used installed agents, existing sign-ins and actual model responses through Codypendent's production daemon. Synthetic provider responses are excluded from the results below.

## What passed

Packaged desktop interaction passed with installed Codex and Ollama's `deepseek-v4.1-flash:cloud`: each generated an interactive answer, created and executed a reusable workspace helper, received real workspace data and completed with prose. In both native checks the rendered counter was clicked from 0 to 1. The native helper ran in the application's WebKit Worker and returned through its original tool call.

The headless matrix separately exercised the real daemon, policy approvals, durable artifacts, desktop MCP continuation and generated helper execution. Its permission-restricted Node host evaluated the exact requested helper source against the real isolated workspace context. This proves the tool roundtrip; it does not establish native widget rendering for every model.

## Installed agent matrix

| Agent/profile | Actual result | Time |
| --- | --- | --- |
| Codex, `gpt-6.1-sol` | Completed: interactive UI artifact, saved helper, successful helper result and final prose. Also passed the packaged native interaction above. | Headless 86.74 s |
| OpenCode, installed default | Completed: interactive UI artifact, saved helper, successful helper result and final prose. | 59.40 s |
| GitHub Copilot, `auto` | All three native desktop tools succeeded, including the generated helper. The bounded check cancelled after 150 s before final prose; complete conversational acceptance is not claimed. Cancellation took 1.04 s. | 151.16 s total |
| Claude Code, `haiku` | Agent startup failed after the 90 s handshake deadline. A separate zero-MCP phase probe received initialize in 14.16 s, then no session/new reply within 45.27 s; no prompt was submitted in that probe. | 95.76 s |
| Gemini, installed default | Agent startup failed after the 90 s handshake deadline. A separate real cancellation check completed cancellation in 1.05 s. | 110.28 s |
| Kimi Code | Agent reported authentication required before a session could start. No tool run. | 21.38 s |
| Cursor, stock installed adapter | Agent reported authentication required and requested its existing `agent login` flow. No tool run. | 39.29 s |
| Cline, installed official 3.0.62 | Production detection selected the existing `cline --acp` CLI without npm. The ACP transport closed during initialize, before any model or tool call. Both installed native binary copies fail strict macOS signature verification; direct help is killed by signal 9. | 14.83 s |

The authoritative core matrix is [backend-matrix.json](../../target/real-agent-acceptance-2026-10-19/backend-matrix.json). The final additional-agent matrix is [additional-agents/summary.json](../../target/real-agent-acceptance-2026-10-19/additional-agents/summary.json). Cline's current daemon result is [cline-current/evidence/acp_cline/summary.json](../../target/real-agent-acceptance-2026-10-19/additional-agents/cline-current/evidence/acp_cline/summary.json), with the installed-binary checks in [cline-installed-diagnosis.json](../../target/real-agent-acceptance-2026-10-19/additional-agents/cline-installed-diagnosis.json).

## Configured HTTP model matrix

| Configured model | Actual result |
| --- | --- |
| `ollama/deepseek-v4.1-flash:cloud` | Packaged native pass: working counter, generated helper Worker roundtrip and Completed prose. The model corrected one initially invalid widget argument shape before succeeding. |
| `ollama/deepseek-v4-pro:cloud` | Headless production tool-loop pass: UI artifact, definition, generated helper roundtrip, Completed; 26.60 s. |
| `ollama/glm-5.2:cloud` | Same complete headless tool sequence; 30.93 s. |
| `ollama/kimi-k3:cloud` | Same complete headless tool sequence; 28.08 s. |
| `ollama/nemotron-3-ultra:cloud` | Same complete headless tool sequence; 30.62 s. |
| `ollama/kev:27b-nvfp4` | Actual chat request rejected: model does not support chat. Ollama metadata reports decision capability; automatic discovery now excludes this profile. |
| `cerebras/gemma-4-31b` | Production readiness check rejected the pin because the provider did not list that model. No completion claimed. |
| `cerebras/gpt-oss-120b` | After the request-format fix, actual generation reached Cerebras and returned HTTP 402 Payment Required: billing/account access is required. No completion or tool pass claimed. |

HTTP evidence is in [corrected-host/summary.json](../../target/real-agent-acceptance-2026-10-19/models/corrected-host/summary.json), [cerebras-fixed/summary.json](../../target/real-agent-acceptance-2026-10-19/models/cerebras-fixed/summary.json) and the initial [models/summary.json](../../target/real-agent-acceptance-2026-10-19/models/summary.json). The initial headless host used a Node flag incompatible with the installed Node version; those custom-helper failures were fixture failures. Four cloud models were rerun successfully with the corrected host; DeepSeek Flash uses its packaged native pass.

## Changes verified by these checks

- ACP adapters prefer the existing vendor CLI through the appropriate override, including `CODEX_PATH` and `CLAUDE_CODE_EXECUTABLE`. Copilot receives its desktop MCP connection in per-launch configuration.
- The native registry offers thirteen workspace capabilities and four builtins. `desktop.executeTool` provides a stable call path for ACP clients that retain their initial tool catalog; generated source, version, arguments and capabilities still pass the native policy gate.
- Hosted requests no longer receive Ollama's `options.num_ctx` field. The Cerebras format failure cleared; its remaining refusal is HTTP 402 billing.
- Discovery lists actual authenticated hosted models, respects installed agents' own default selection, and filters confirmed non-chat or retired local profiles. It does not generate responses, install packages or start sign-in flows.
- Chat shows readable action summaries, approvals and provider errors, including the desktop MCP transport aliases used by installed agents. Exact arguments, source, wire names and original tool types are available through explicit details. Requested JSON and fenced code remain intact.
- ACP reasoning events remain in durable history while flagged thoughts are excluded from the final-answer summary. The SQLite migration ledger upgrade passed its regression and six upgrade assertions.
- Live user submissions can try at most two confirmed, untried alternatives after a provider/model/startup failure before any tool attempt, proposal, approval, question or patch. Historical failures, cancellation, pause, ordinary task failure and policy/budget/classification limits do not trigger replay. Draft, model, connection or chat changes invalidate pending recovery. Successful replacement folds the earlier connection failures into details. This path has focused automated coverage; native fallback acceptance is recorded separately.

## Focused verification

Counts describe separate scoped runs, not a full test sweep.

| Check | Result |
| --- | --- |
| Recovery guards, early acceptance race, bounded alternatives, billing, policy limits and StrictMode cleanup | 22 passed |
| Chat, JSON preservation, legacy error presentation, approvals and result summaries | 18 passed |
| Chat tool labels, including trusted desktop MCP prefixes and transport aliases | 5 passed |
| Failure classification and credential sanitization, including billing precedence | 45 passed |
| App integration and navigation | 14 passed |
| Native AG-UI adapter and interactive/tool surfaces | 26 passed |
| Final native discovery | 10 passed; 1 live-machine scan ignored |
| Final native launcher | 12 passed |
| Discovery/readiness/onboarding frontend | 10 passed across three files |
| Added backend execute-tool, provider-error, Copilot-launch, hosted-options and cancellation checks | 7 passed |
| SQLite ACP sink preserves reasoning events and excludes thoughts from the final-answer summary | 1 passed |
| Installed Cline CLI resolver | 1 passed |
| SQLite migration ledger regression and upgrade checks | 1 passed; 6 upgrade assertions passed |
| Desktop TypeScript and production frontend build | Passed |

## Native evidence

- [Native run and artifact evidence](../../target/native-realagents-20261019/evidence.json).
- [Final native tool cards](../../target/native-realagents-20261019/final-native-tool-cards.png).
- [Completed Codex chat](../../target/native-realagents-20261019/codex-completed-chat.png) and its native accessibility capture in the same directory.
- [Ollama interactive counter](../../target/native-realagents-20261019/ollama-deepseek-widget.png).
- [Completed automatic recovery](../../target/native-realagents-20261019/automatic-recovery-completed.png).
- [Readable historical provider error](../../target/native-realagents-20261019/historical-provider-error-readable.png).

The native automatic-fallback check selected the isolated `acceptance/unavailable` profile at `http://127.0.0.1:1` and submitted a short objective requiring no tools. After the real connection refusal, the same chat automatically retried with `ollama/deepseek-v4.1-flash:cloud` and completed with a plain-language reply, without intervention. The Trying status and changed selection were visible; the failed attempt became a closed Earlier model connection attempt disclosure. Session `01a153c7-3a84-72a0-95a8-737fd2e74b61` contains failed run `01a153c7-3ab9-7d00-95f9-107d164d8d51` and completed replacement `01a153c7-bd27-70d2-9fcb-d760c8dcd0ce`.

Native restoration also passed: the saved Codex widget remained interactive and its technical JSON stayed closed. Historical Codex provider error envelopes previously saved under Completed now show a readable error with the original payload behind closed Technical details.

## Installed application and Fold identity

The complete `/Applications/Codypendent.app` bundle was replaced after its previous installation was backed up and verified. The installed CLI, worker and bundled Node runtime match the new bundle; application registration and the installed CLI's idle-guard daemon restart passed. Launching the absolute installed path produced both desktop and daemon processes from that installed bundle. The original `jev-investigation` repository remained selected, the available `acp/codex-acp` agent was chosen automatically, and the workspace connected without setup interaction.

The running installed application's About window visibly uses the Fold icon. Evidence is [installation.json](../../target/dock-branding-2026-10-19/installation.json), [installed runtime Fold icon](../../target/dock-branding-2026-10-19/installed-runtime-fold-icon.png), and [installed connected workspace](../../target/dock-branding-2026-10-19/installed-connected.png) with its [accessibility capture](../../target/dock-branding-2026-10-19/installed-connected.ax.txt). Direct Dock automation was unavailable, so literal Dock pixels were not verified.

## Follow-up provider and Monty acceptance

The subsequent [provider coverage pass](2026-10-19-provider-coverage.md) adds actual ZCode Flash completion through the existing signed-in account, Amp metadata acceptance, official Antigravity initialization with an explicit authentication boundary, and a working official Cline release in the app cache. Earlier timeout and authentication results above remain historical observations. The separate [Monty native check](2026-10-19-monty-sandbox.md) verifies a model-created Python helper executing against actual workspace context, compact approvals and a plain-language completed response.
