# Phase 6 — Plugin and Multimodal Ecosystem

> **Objective:** a governed plugin system (MCP servers, WASM components, sandboxed native processes) with permission-diff updates; voice and image input; semantic themes; and the agentic setup assistant — all under the security model, none of it trusted by default.
>
> **Specification chapters:** [Roadmap Phase 6](../15-roadmap.md), [Skills, Tools, and Plugins](../05-skills-tools-and-plugins.md), [Security and Governance](../11-security-and-governance.md), [IDE, GitHub, and Multimodal Integration](../10-ide-github-and-inputs.md). Example manifests: [`specs/plugin.toml`](../../specs/plugin.toml), [`specs/hook.toml`](../../specs/hook.toml), [`specs/runner.toml`](../../specs/runner.toml).
>
> **Exit criteria (from the roadmap):** a plugin cannot access an undeclared path; permission expansion on update requires approval; original audio/image artifacts remain linked; the setup assistant proposes rather than silently changes sensitive configuration.

## New crate

**EDIT FILE `Cargo.toml`** — add member `"crates/sandbox"` (`codypendent-sandbox`): capability-grant materialization, process isolation, WASM runtime. This is the crate that justifies its existence with a **security boundary** (the manual's crate rule). Heavy deps (`wasmtime`) live here only.

## STEP 6.1 — Plugin manifests and lifecycle

1. Parse `plugin.toml` with exactly the [`specs/plugin.toml`](../../specs/plugin.toml) shape: id, name, version, kind (`native-process` | `wasm-component` | `mcp-remote`), publisher, scopes, `[runtime]` (command, protocol, working_directory), `[capabilities]` (filesystem_read/write, network allowlist, secrets, subprocess), `[resources]` (memory/cpu/wall/output caps), `[security]` (checksum, signature, sandbox_profile), `[update]`.
2. Implement the [Chapter 05](../05-skills-tools-and-plugins.md) lifecycle **exactly**: discover → inspect manifest → verify signature/checksum (sha256 of the artifact; signature via `ed25519-dalek` against the publisher key — unsigned plugins follow policy `[plugins].unsigned`, default deny) → evaluate permissions (render the capability list to the user) → **install disabled** → sandbox smoke test (start, handshake, list tools, stop — inside the sandbox) → user enables at a chosen scope → monitor (resource usage, crashes) → update with **permission diff** → revoke/remove.
3. **Update rule (exit criterion 2):** an update whose manifest requests any capability not in the installed set is blocked until explicitly re-approved; the TUI shows the diff (`+ network: uploads.github.com:443`).
4. Registry integration: plugin-provided tools/prompts register as registry items with `provenance = plugin(<id>@<version>)` and the plugin's trust tier; **semantic relevance never implies trust** — retrieval hard-filters on trust exactly as Phase 2 built it.

**TESTS** — checksum mismatch rejected; unsigned + default policy rejected; disabled plugin's tools not retrievable; permission-expansion update blocked; permission-identical update auto-allowed on `stable` channel.

**COMMIT** `"phase6: plugin manifests, verification, lifecycle, permission-diff updates"`

## STEP 6.2 — Native process sandbox and MCP host

1. Sandbox profiles per platform ([Chapter 11](../11-security-and-governance.md)): clean environment (no inherited vars beyond an allowlist), pre-opened working directory only, network via allowlist (Linux: user namespaces + seccomp where available, else document degraded mode loudly at install time; macOS: `sandbox-exec` profile), rlimits for memory/CPU, wall-clock kill, process-group termination, output caps → artifacts. **No inherited file descriptors, no secrets in env** — secrets are brokered per call ([Chapter 11](../11-security-and-governance.md) secrets: OS keychain via the `keyring` crate; DB stores identifiers only).
2. MCP host: connect stdio MCP servers as `native-process` plugins through the framework's MCP integration ([Chapter 12](../12-agent-framework-rs-integration.md)), inside the sandbox. Tool outputs are **sanitized untrusted content**: labeled by origin, size-capped, control-sequence-stripped before entering context ([Chapter 11](../11-security-and-governance.md) prompt-injection handling). MCP is a protocol, not a trust guarantee — the manifest still declares capabilities, and undeclared access fails.
3. The exit-criterion test (1): a test plugin that tries to read `$HOME/.ssh/id_rsa` and to connect to an un-allowlisted host must fail both, with audit events recording the denials.

**TESTS** — undeclared path read fails; undeclared network fails; env canary invisible; resource-cap kill; malicious MCP output (ANSI escapes + injection text) arrives labeled and stripped; secret broker passes a token to a tool without the model or event stream ever containing it (scan test).

**COMMIT** `"phase6: native sandbox, brokered secrets, hardened mcp host"`

## STEP 6.3 — WASM component plugins

1. `codypendent-sandbox` embeds `wasmtime` (component model): define the initial WIT world `codypendent:plugin` with imports limited to: logging, brokered KV (plugin-scoped), granted-path file read, granted-host HTTP — every import backed by the capability grant; no WASI ambient filesystem/network.
2. Resource metering: fuel + memory limits from `[resources]`; a trapped/exhausted plugin unloads cleanly without daemon impact.
3. SDK: `sdk/wasm-plugin/` template crate + `codypendent plugin new --wasm` scaffold; one example plugin (`examples/plugins/word-count/`) exercising a tool round-trip.
4. WASM is the **preferred** runtime for new Codypendent-native plugins ([Chapter 05](../05-skills-tools-and-plugins.md)); the docs template says so.

**TESTS** — undeclared import fails at load; fuel exhaustion traps cleanly; example plugin tool call round-trips.

## STEP 6.4 — Hooks and skill scripts become executable

Phases 2–5 recorded hooks and skill scripts without executing them. Now wire execution through the sandbox:

1. Hook engine per [`specs/hook.toml`](../../specs/hook.toml) and [Chapter 20](../20-interaction-and-autonomy-model.md): events (`patch.proposed`, `run.completed`, plan changes, tool lifecycle), kinds (`observe|transform|validate|authorize|notify|agent-evaluate`), runtimes (command in sandbox, WASM, HTTP, prompt-evaluator), `failure` policy (`block|warn|ignore`), priority ordering, capability grants per hook. An agent-evaluate hook **cannot grant its own capabilities**.
2. Skill `scripts/` execute as sandboxed commands under the skill's declared `[permissions]` — the Phase 2 restriction lifts; the registry flag flips.

**TESTS** — validate-hook blocks a failing patch (the [`specs/hook.toml`](../../specs/hook.toml) cargo-test example, end to end); authorize-hook denial is audited; hook capability escalation attempt fails.

**COMMIT** `"phase6: sandboxed hooks and skill scripts"`

## STEP 6.5 — Multimodal input

Implement the [Chapter 10](../10-ide-github-and-inputs.md) `InputEnvelope`/`InputBlock` model (protocol addition; blocks: Text, Audio, Image, File, EditorSelection, CodeSymbol, GitHubReference):

1. **Images:** TUI clipboard paste (OSC 52 / terminal protocols where available) and `codypendent attach-image <path>`; IDE extension drag-drop. Pipeline preserves, as linked artifacts: (1) original image, (2) extracted text (OCR optional, feature-gated), (3) model observations, (4) crop/coordinate references. **The original is never replaced by a summary** (exit criterion 3).
2. **Voice:** push-to-talk in the TUI (configurable key), streaming or post-record transcription via a configured transcription model (`models.toml` entry; local whisper-server counts as local policy), transcript **review before submission** (the user confirms/edits), deterministic voice commands ("approve", "cancel run") matched before free text, optional TTS out (feature-gated). Original audio artifact kept where policy allows.
3. Data classification applies: image/audio artifacts default `Confidential`; remote transcription requires policy allowing that classification off-device.

**TESTS** — image envelope round-trip with all four linked artifacts; audio artifact retained + linked to transcript; classification gate blocks remote transcription under a restrictive policy.

## STEP 6.6 — Themes and theme packs

Finish the [Chapter 10](../10-ide-github-and-inputs.md) theme system: semantic tokens only (surface/text/status/syntax/diff/agent/focus/selection); ship true-color, 256-color, 16-color, monochrome, high-contrast, and color-blind-safe variants; theme packs are data-only plugins — **theme plugins must not receive execution permissions** (README rule; enforce by rejecting a theme manifest with any capability). Terminal capability detection picks the best variant with manual override.

**TESTS** — snapshot per variant; theme-with-capabilities rejected; 16-color fallback renders every widget legibly (snapshot).

## STEP 6.7 — Agentic setup assistant

`codypendent setup` — an agent run under a dedicated restricted profile (the root [README's "Agentic Setup & Personalization" section](../../../README.md) is the specification for its constraints):

1. Discovers: installed toolchains, repo layout, existing `AGENTS.md`/`CLAUDE.md`/Cursor rules/MCP configs (compatibility importers — normalize into the registry with source provenance, per [Chapter 19](../19-competitive-design-synthesis.md)), available local models (probe Ollama), terminal capabilities.
2. **Proposes** a configuration change-set (config diffs rendered like code diffs) — policy, models.toml, imported items, theme. The user accepts per item.
3. Hard MUST-NOTs (enforced by its profile, not by prompt): never silently install executable plugins, never broaden permissions, never read secret stores, never weaken privacy routing, never alter org policy (exit criterion 4 — the profile denies these capabilities so the attempt fails structurally).

**TESTS** — setup run's grant excludes plugin-install/secret capabilities (assert denial events on attempt); import of a fixture `AGENTS.md` lands as a scoped registry item with provenance; accepted config diff applies, rejected leaves no trace.

## Exit checklist

- [ ] Sandboxed plugin reading an undeclared path or host fails with audit events (exit criterion 1).
- [ ] Plugin update adding a capability blocks until re-approval; the diff is displayed (exit criterion 2).
- [ ] Pasted image and recorded audio keep their original artifacts linked through transcript/observation chains (exit criterion 3).
- [ ] Setup assistant proposes diffs; sensitive changes structurally impossible under its profile (exit criterion 4).
- [ ] The `specs/hook.toml` verify-after-patch hook blocks a failing patch end to end.
- [ ] One WASM example plugin and one sandboxed MCP server run under caps.
- [ ] Six theme variants render; themes carry zero execution permissions.
- [ ] `fmt` / `clippy` / `test` green; commits made; tree clean.
