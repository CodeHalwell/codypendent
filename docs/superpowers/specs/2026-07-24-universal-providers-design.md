# Universal model providers + ACP — design (foundation)

**Date:** 2026-07-24 · **Status:** approved (pre-implementation) · **Branch:** `claude/universal-providers` (off `main`)

Research reference: `scratchpad/provider-research.md` (~40-provider table, auth mechanics, ACP, cited sources).

## Problem

Today the runtime accepts only `provider = "openai-compatible"` with an `api_key_env`
(`crates/runtime/src/models.rs` `ModelConfig`). There is no way to add Azure/Bedrock/Vertex,
no cloud-IAM auth, no OAuth, and no path to agent products (Cursor, OpenCode, Claude Code,
Codex, Gemini) that own their own model. The user wants **any model from any provider,
directly or via ACP**.

## Findings that shape the design (from research)

- **~30 of ~40 direct providers are config-only OpenAI-Chat-compatible** — `base_url` +
  `Authorization: Bearer <key>` + `/chat/completions`. Today's `ModelConfig` already
  reaches them; the near-term win is a **curated catalog**, not new wire code.
- **Auth has exactly three mechanical shapes** (ascending difficulty): **static header**
  (≈all providers) → **refreshing token** (subscription OAuth, Azure Entra, GCP ADC,
  short-term Bedrock) → **request signing** (AWS SigV4 only). ⇒ model auth as a
  **credential-provider trait, not a `String`**.
- **ACP is a different axis:** an ACP agent is an autonomous subprocess you delegate a
  turn to (JSON-RPC 2.0 over stdio, `protocolVersion: 1`); it **owns its model**. Rust is
  ACP's reference language (`agent-client-protocol` crate). This is the **sanctioned way
  to use a Claude Code / ChatGPT / Gemini subscription** — connect to the official agent,
  which uses the user's real login. Connect via subprocess: `gemini --acp`,
  `opencode acp`, `agent acp`, `npx @agentclientprotocol/{claude-agent,codex}-acp`.
- **Subscription-OAuth reuse (reverse-engineered ChatGPT/Claude API OAuth) violates ToS**
  and Anthropic enforces bans (2026). **Not built** (user decision "ACP now, revisit OAuth
  later"): the auth enum reserves an `OAuth` variant, documented opt-in/at-risk, but no
  reverse-engineered flow ships. Official OAuth (where a provider supports it) is fine.

## Scope (this program — user chose "Foundation + API-key + ACP")

**In:** the provider/auth abstraction; a curated ~40-provider API-key/OpenAI-compatible
catalog + a data-driven credential-provider trait (ApiKey + the shape for CloudIam/OAuth);
the **ACP client** (connect to an external agent, delegate a turn, stream its output back
through the existing run/event model); the TUI picker over the catalog (the earlier
`model-provider-selection.md` note, now buildable). **Out (follow-up PRs):** concrete
cloud-IAM signing (AWS SigV4 / GCP ADC / Azure Entra token refresh) beyond the trait shape;
any reverse-engineered subscription OAuth; the full picker auth state-machine polish.

## Data model (backward-compatible superset of `models.toml`)

```rust
// crates/routing or a new crates/providers — a catalog, not secrets.
pub struct Provider {
    pub id: String,                 // "openai", "azure", "bedrock", "ollama", "acp:claude-code"
    pub protocol: Protocol,         // EXPLICIT (research: every prior-art system regrets leaving it implicit)
    pub base_url: Option<String>,
    pub auth: Vec<AuthMethod>,      // a Vec — Azure/Bedrock/Anthropic legitimately offer several ("paste a key OR log in")
    pub extra_headers: BTreeMap<String,String>,
    pub query_params: BTreeMap<String,String>,
}
pub enum Protocol { OpenAiChat, Anthropic, GeminiNative, Acp }   // extensible, non_exhaustive
pub enum AuthMethod {
    None,
    ApiKey { env: String },                 // secret referenced by env-var NAME, never persisted (today's invariant)
    CloudIam(CloudIam),                     // AwsSigV4 | GcpAdc | AzureEntra — trait shape now, signing in a follow-up
    Acp { command: Vec<String> },           // launch line for the agent subprocess
    OAuth(OAuthKind),                       // RESERVED, opt-in, not wired (ToS caveat) — no reverse-engineered flow ships
}
pub struct Model { pub id: String, pub provider_id: String, /* + optional catalog metadata: cost, ctx, caps, location */ }
```

Secrets stay **referenced by env-var name and never stored** (preserves the current
`api_key_env` invariant). The whole thing is a superset of today's `[[model]]` table — an
existing `models.toml` keeps working (its entries map to `Provider{protocol: OpenAiChat,
auth:[ApiKey{env}]}`).

## Architecture

1. **Credential-provider trait** (`crates/runtime` or new `crates/providers`):
   `trait CredentialProvider { async fn headers(&self, req) -> Result<HeaderMap>; }` with an
   `ApiKey` impl now and the `CloudIam`/`OAuth` impls stubbed to the trait (real signing/refresh
   in follow-ups). The existing `client_for` (`models.rs:219`) is generalized to build a client
   from `Provider.protocol` + a `CredentialProvider`, not a hard-coded openai-compatible client.

2. **Provider catalog**: a curated, versioned built-in list of the ~40 providers (id, protocol,
   base_url, default auth method) shipped in-repo, shadowable/extendable by the user's
   `providers.toml` / `models.toml`. The routing `ModelProfile`/`models bench` layer plugs into
   this (measured metadata over catalog metadata).

3. **ACP client** (`crates/integrations` or new `crates/acp`): use the `agent-client-protocol`
   Rust crate; spawn the agent subprocess (`Provider.auth = Acp{command}`), do the
   `initialize`/session handshake, delegate the run's objective as an ACP prompt, and **map the
   agent's ACP updates back onto Codypendent's existing event stream** (`ModelStreamDelta`,
   tool/patch events) so the TUI renders an ACP-backed turn identically to a native one. The
   agent owns its model — Codypendent sends no model id, optionally probing the agent's model
   list. Approval/permission requests from the agent map onto the existing approval flow.

4. **TUI picker** (`crates/tui`): the `model-provider-selection.md` picker — a `/model` /
   `/provider` palette surface over the catalog + connected providers, with the classification
   badges the routing layer already carries. Selecting an ACP provider connects to that agent.

5. **Protocol**: additive only — a run/model may name a `provider_id`; ACP-backed runs reuse
   the existing event model (no new wire events; ACP updates are translated daemon-side).

## Non-goals

- No reverse-engineered subscription OAuth (ToS/ban risk) — ACP is the sanctioned substitute.
- No full cloud-IAM signing in this PR (trait shape only; AWS SigV4 / GCP ADC / Azure Entra
  refresh are follow-up PRs).
- Not rewriting routing/eval — the catalog feeds the existing `ModelProfile` layer.

## Testing

- Catalog: an existing `models.toml` maps to the new `Provider`/`Model` model unchanged (back-compat);
  a new provider (e.g. Azure/Groq) parses.
- CredentialProvider: `ApiKey` produces the right `Authorization` header from the env var;
  a missing env var errors (preserves today's `MissingApiKeyEnv`).
- ACP client: against a mock ACP agent (a scripted stdio JSON-RPC peer), the handshake completes,
  a prompt is delegated, and streamed updates map onto `ModelStreamDelta`/tool events; an agent
  permission request maps onto an approval.
- Picker: renders catalog providers + models; selecting one sets the run's provider.
- All existing tests green; golden vectors updated only if a wire type changes (additive).

## Constraints

- Secrets referenced by env-var name, never persisted.
- Additive protocol; existing `models.toml` keeps working.
- Preserve the routing classification hard-filter (a hosted provider is still gated by data
  classification) and the T1/T7 cost honesty.
- `agent-client-protocol` is the only new external dep (Rust reference impl); vet its licence
  under the existing `cargo deny` gate.

## Open questions / risks

- **Where the catalog lives** (extend `crates/routing`, or a new `crates/providers` crate) — the
  plan pins it; a new focused crate is likely cleanest given the size.
- **ACP↔event mapping fidelity** — ACP's update taxonomy vs Codypendent's events; the plan pins the
  mapping and tests it against a mock agent.
- **Cloud-IAM depth** — this PR ships the trait + ApiKey; SigV4/ADC/Entra are sequenced follow-ups
  (research: SigV4 hardest; Bedrock bearer keys + Azure `/openai/v1/` reduce the common-case pain).
