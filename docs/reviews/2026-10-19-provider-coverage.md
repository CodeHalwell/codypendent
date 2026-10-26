# Automatic provider coverage — 19 October 2026

Codypendent discovers installed agents and verified cached adapters, reuses their existing sign-ins, and preserves their advertised model identifiers. OpenAI, Anthropic and Gemini also have direct HTTP drivers; their existing configured keys and account model lists are discovered without an agent adapter.

| Requested provider | Integration | Current evidence |
| --- | --- | --- |
| OpenAI | Native OpenAI HTTP and installed Codex ACP | Codex `gpt-6.1-sol` completed the earlier desktop-tool acceptance run. Direct HTTP driver coverage is implemented; this pass did not repeat OpenAI HTTP generation. |
| Anthropic | Native Anthropic HTTP and installed Claude Code ACP | Installed Claude adapter remains supported; its earlier bounded session startup timed out. This pass did not repeat Anthropic HTTP generation. |
| Gemini | Native Gemini HTTP and installed Gemini CLI ACP | Installed Gemini adapter remains supported; its earlier bounded session startup timed out. This pass did not repeat Gemini HTTP generation. |
| Antigravity | Official Google `antigravity-acp` runtime | Official 1.4.1 installed through the production installer. Initialize succeeded; `session/new` explicitly returned `Authentication required` in 10.45 seconds. |
| GitHub Copilot | Installed CLI ACP, per-launch MCP configuration | Earlier actual desktop-tool roundtrip passed; its final assistant response exceeded the run budget. |
| OpenCode | Installed native ACP | Earlier actual desktop-tool run completed. Flagged thoughts are now excluded from its final-answer summary. |
| Cline | Healthy installed CLI, then verified app-owned binary cache | Official 3.0.73 ARM64 archive and unchanged code signature verified. Exact final cache path initialized ACP in 3.10 seconds. Generation was not exercised in this pass. |
| Amp | Installed/cached ACP adapter plus installed Amp CLI | Cached adapter 0.9.0 initialized and opened a session in 2.36 seconds. Generation was not exercised in this pass. |
| ZCode | Embedded ACP bridge to the installed app's own `app-server` | Actual `account:zai-individual-coding-plan/GLM-5.3-Flash` returned a normal assistant sentence and completed in 19.50 seconds. No tools were requested in that response probe. |
| Kimi | Installed Kimi Code native ACP | Earlier real run reached the agent's sign-in requirement; integration remains supported. |

The earlier full model/agent matrix is recorded in [real-agent acceptance](2026-10-19-real-agent-acceptance.md). The Antigravity and ZCode results above are separate from native WebKit/generated-widget acceptance.

## Discovery and launch

The ACP registry now enumerates verified cached binary snapshots as well as local executables and cached npm adapters. A catalog refresh no longer hides an older, already installed adapter. An unhealthy installed Cline executable falls through to its verified cached release before attempting npm. The staged Cline snapshot preserves the original `package/bin/cline` archive layout and supporting assets.

Amp receives its documented `AMP_CLI_PATH` override. Official Antigravity uses `agy_acp_server.par` and its matching `localharness_external` companion. When the Google CLI is installed, the official companion can be prepared on first use. Download and extraction retain existing size/path bounds; cancellation stops preparation before project tools run. The registry omits Google's archive checksum, so this narrowly scoped path requires Google's fixed HTTPS publisher and rejects redirects to other hosts, then records the downloaded archive's SHA-256.

The legacy bridge has the separate `antigravity-community` identity. Passive discovery excludes it and old community snapshots even if an earlier snapshot used the official-looking ID. CLI, TUI and desktop retain explicit consent for that legacy bridge; Google's official runtime does not inherit its warning.

Every requested agent appears in the provider catalog even when its executable is unavailable. Readiness still distinguishes an installed runtime, a successful handshake, and a successful model response. Installed agents now retain a canonical Default profile when background metadata times out or fails, so a slow Gemini startup no longer removes it from Models. These fallback profiles do not acquire a connected status and are excluded from automatic default selection. Existing choices remain intact; actual use retries the installed agent with its existing sign-in. A focused deterministic timeout/fallback regression passed 1/1.

## ZCode protocol and account reuse

The embedded bridge launches the installed `zcode.cjs app-server` over local pipes. It implements the actual ZCode envelope without a `jsonrpc` field, subscribes using `desktop-continuous`, forwards the run's MCP servers, and translates text, thoughts, tool events, permission requests and cancellation into ACP. New helpers continue through the existing Codypendent MCP dispatcher and its policy gates.

The bridge passes the installed app's provider configuration paths without rewriting them. It projects the desktop's own nonsecret plan verdict and built-in provider metadata, preserves a current model when one is returned, and otherwise selects an advertised model without persisting a workspace preference. The real probe explicitly selected the advertised Flash model through ACP.

Account credentials are brokered only in memory to the installed backend for an active owned chat, its selected advertised provider/model, a positive plan verdict, and an approved Z.AI/BigModel vendor origin. The encrypted credential format matches the installed ZCode implementation. Ambiguous account keys and unrelated sessions/providers/origins are refused. Credentials never enter ACP responses, tool results, configuration writes or logs. Start-plan captcha and unrelated interaction types are not bypassed.

## Verification

- ACP registry focused suite: **18 passed**.
- ZCode local-pipe protocol and secret-broker regressions: **2 passed**. These cover MCP forwarding, explicit one-time permissions, cancellation, duplicate-stream suppression, thought separation, and sentinel-key exclusion from ACP output.
- Actual ZCode metadata and Flash response, official Antigravity metadata/auth boundary, Amp metadata, and exact Cline cache initialization are distinguished in the table above.
- Rust formatting, JavaScript syntax and scoped diff checks passed. Final CLI/native packaging is performed by the parent acceptance lane.

Evidence: `target/provider-coverage-2026-10-19/zcode-flash-response.json`, `target/provider-coverage-2026-10-19/antigravity-metadata.json`, and `target/cline-distribution-probe-2026-10-19/final-cache-acp-repeat.json`.

Sources: [official ACP registry](https://github.com/agentclientprotocol/registry), [Amp adapter launch contract](https://github.com/tao12345666333/amp-acp), [official ZCode app-server source](https://github.com/zai-org/ZCode/tree/main/apps/zcode-cli), and the [ZCode ACP reference bridge](https://github.com/william0wang/zcode-acp).

## Later connection recovery

The [later production checks](2026-10-19-auth-recovery.md) supersede the earlier Claude/Gemini/Antigravity failures in this report. All three completed real desktop-tool runs using existing sign-ins. Google’s retired Gemini route recovered through the same cached account and an advertised Gemini model in official Antigravity. Claude’s old `opus[1m]` selector is no longer advertised. Kimi’s saved token files are empty; repeating its metadata scan cannot recover a login that is absent. Background metadata discovery now allows 45 seconds for the measured slow Claude and Gemini handshakes; other agents retain their eight-second deadline.
