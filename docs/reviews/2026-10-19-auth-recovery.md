# Installed account recovery — 19 October 2026

Claude, official Google Antigravity and Gemini Default completed real desktop-tool runs using their existing sign-ins. Kimi's current and legacy credential stores contained no usable saved login; its native agent correctly requested authentication.

| Profile | Current actual result | Time |
| --- | --- | --- |
| Claude Code, `claude-acp#haiku` | Completed through the production daemon: UI artifact, saved helper, successful workspace result and final prose. Current adapter 0.76.0 uses the installed Claude executable. | 58.97 s |
| Official Antigravity, installed default | Completed through the production daemon with the same three desktop-tool steps and actual workspace title/repository in final prose. | 90.56 s |
| Gemini Default | Google's retired individual-client response triggered automatic recovery through the same saved Google account. The replacement advertised and selected `gemini-3.8-flash`; UI artifact, helper definition, helper execution and final prose completed. | 111.89 s |
| Kimi Code 0.39.1 | Initialized, then returned Authentication required. Current access/refresh token fields were blank, the legacy OAuth file was empty, and no other credentialed profile was configured. No login was started. | 1.02 s |

The successful model runs repaired malformed tool arguments before completion. These checks verify the production daemon, approvals, private desktop MCP connection, durable artifacts and actual generated-helper results. They use the bounded headless acceptance host; native WebKit rendering is a separate check.

The screenshot's exact `opus[1m]` pin received a separate bounded metadata check. Current Claude adapter 0.76.0 initialized and opened a session in 13.18 seconds, but did not advertise that selector. Its current Opus choices were `opus` (Opus 5.5), `claude-opus-5`, `claude-opus-4-8`, `claude-opus-4-7` and `claude-opus-4-6`. No replacement was selected and no prompt was submitted. This identifies the stale pin; it does not infer those models' context capacities. [Exact-selector evidence](../../target/auth-recovery-2026-10-19/claude-opus-large-context.json).

## Causes and fixes

Claude's earlier timeout did not reproduce on the current integration. Its current adapter initialized in 0.47 seconds and opened a session in 24.86 seconds; the full tool run above then passed without disabling user plugins or extending the timeout.

Both installed Gemini CLI 0.46.0 and cached 0.58.0 reached an explicit Google service response retiring Gemini Code Assist for individual accounts. Replacing the CLI version or repeating sign-in would not repair that route. Recovery is limited to that retirement response, a matching cached Google subject/issuer, an existing official Antigravity login, and an advertised Gemini model. Explicit model selections require an exact advertised match. Recovery happens before the prompt and tools; unavailable matches preserve Google's original remedy. The effective agent/model and a concise migration note are recorded in the durable run.

Official Antigravity requires ACP authenticate before session creation. The client now sends only its advertised oauth-personal method when a bounded, valid existing consumer profile is present, and rechecks the profile immediately before authentication. Tokens never enter ACP requests or Codypendent configuration. Missing, malformed or removed credentials do not initiate sign-in. Timeout errors identify initialization, saved sign-in or session creation; unrelated failures no longer acquire misleading sign-in advice.

Its inherited IDE MCP configuration also contained a dead data-agent-kit socket. Codypendent now supplies a private temporary GEMINI_HOME with its own MCP context. The existing antigravity-acp directory is linked as a whole, preserving native settings, trust, history and atomic credential refresh; non-MCP configuration, rules and skills remain linked. The original global MCP file is unchanged. Google's normal macOS keychain identity remains unchanged, and no credentials are copied. A separate real response check completed in 6.09 seconds after isolation.

Current and legacy ACP model declarations are both supported. Legacy model state is retained before schema deserialization can discard it, and advertised legacy selections use session/set_model. Current configOptions take precedence. Model identifiers are bounded and never invented.

CLI connect and probe now use the same cached-authentication options as desktop launches. Probe applies an exact advertised model pin before submitting its prompt and stops on an unknown or refused selection.

## Verification and evidence

- Scoped ACP client, mapping, registry and Google-home checks: **69 passed**. Coverage includes authentication ordering, sentinel exclusion, invalid/missing credentials, same-account guards, current/legacy model selection, cancellation, and atomic refresh through the directory link.
- CLI exact-selector protocol regression: **1 passed**. Successful selection, an absent large-context pin and a refused selection never fall through to a default-model prompt.
- Three real production desktop-tool runs above: **3 completed**. Both isolated acceptance daemons stopped normally afterward.
- Final CLI and native builds passed in the integration lane; scoped diff checks passed.

[Production summary](../../target/auth-recovery-2026-10-19/production-summary.json), [Claude run](../../target/auth-recovery-2026-10-19/claude-native-mcp/evidence/acp_claude-acp_haiku/summary.json), [Antigravity run](../../target/auth-recovery-2026-10-19/google-native-mcp/evidence/acp_antigravity-acp/summary.json), [Gemini recovery run](../../target/auth-recovery-2026-10-19/google-native-mcp/evidence/acp_gemini/summary.json), [isolated Google response](../../target/auth-recovery-2026-10-19/antigravity-isolated-response.json), [Kimi credential audit](../../target/auth-recovery-2026-10-19/kimi-login-audit.json).

Sources: installed official adapter/runtime implementations; Google's [authentication contract](https://antigravity.google/docs/install/) and [MCP configuration](https://antigravity.google/docs/mcp/); Kimi's official [credential storage](https://github.com/MoonshotAI/kimi-code/blob/main/packages/oauth/src/storage.ts) and [ACP server](https://github.com/MoonshotAI/kimi-code/blob/main/packages/acp-server/src/server.ts).

## Approved current Opus check

The approved `claude-acp#opus` selector completed one real, tool-denied prompt through the final CLI in **16.85 seconds** (exit 0, EndTurn). It returned `OPUS CONNECTED`, omitting the requested final full stop. Exact advertised selection was applied before the prompt; no substitute model or profile write was used. This launch resolved Claude Code adapter **0.73.0** and advertised `opus`; this response check does not infer a context capacity from that alias. [Sanitized response evidence](../../target/auth-recovery-2026-10-19/claude-opus-approved-response.json).
