# Codypendent — Universal Model-Provider & Auth Abstraction (Research Reference)

Research date: **July 2026**. All facts verified against live first-party docs, the ACP
reference schema, crates.io/npm registries, and well-known reverse-engineering sources.
Where a value is reverse-engineered or could not be single-source-confirmed it is labelled.

## Where Codypendent is today

`crates/runtime/src/models.rs` models exactly one provider kind. `ModelConfig` =
`{ id, provider: "openai-compatible", base_url, model, api_key_env }`, loaded from
`<config_dir>/codypendent/models.toml`; `ModelRegistry::client_for` builds an
`OpenAIChatCompletionClient::new(key, model).with_base_url(base_url)`. Two invariants worth
preserving into the new design:

1. **`api_key_env` holds the environment-variable *NAME*, never the secret.** The value is
   read at call time and never persisted, logged, or placed in model context (Chapter 11).
2. Non-`"openai-compatible"` providers are hard-rejected today (`UnsupportedProvider`).

The abstraction below is a strict superset: `"openai-compatible" + api_key_env` becomes one
`(protocol = OpenAiChat, auth = ApiKey{env})` case, and everything else is new variants.

---

## Executive summary — key design implications

1. **~30 of 35 direct providers are config-only OpenAI-Chat-compatible** — `base_url` + static
   `Authorization: Bearer` + `/chat/completions`. Codypendent's current shape already covers
   them; the win is mostly a curated catalog, not new code.
2. **Four things break the naive "append `/v1`, static key" assumption** and are the real work:
   (a) **non-standard base paths** (Cohere `/compatibility/v1`, Zhipu `/api/paas/v4`,
   Perplexity no-`/v1`, DeepInfra `/v1/openai`, GitHub Models `/inference`); (b) **model-id
   rewriting** (Azure deployment names, Bedrock/Vertex ids, namespaced slugs); (c) **native
   wire bodies** (Anthropic Messages, Gemini `generateContent`, Bedrock Converse, Cohere v2);
   (d) **non-string auth** (below).
3. **Auth has exactly three mechanical shapes**, in ascending difficulty:
   **(i) static header** (one `String`, ~all providers) → **(ii) refreshing token**
   (subscription OAuth + Azure Entra + GCP ADC + Bedrock short-term; needs a token store,
   expiry, refresh; Copilot/Gemini add a *two-step* identity→inference exchange) →
   **(iii) request signing** (AWS SigV4 — the only one that must sign every request; service
   `bedrock`). Model auth as a **credential-provider trait**, not a `String`.
4. **AWS SigV4 is the single hardest auth** to implement — but 2025's **Bedrock bearer API
   keys** (`AWS_BEARER_TOKEN_BEDROCK`) + the Dec-2025 **`bedrock-mantle` OpenAI-compatible
   endpoint** let you reach Bedrock with a static key over `/chat/completions`, sidestepping
   SigV4 entirely for the common case. Vertex is the hardest *unavoidable* case (OAuth token
   even on its OpenAI-compat path).
5. **Subscription OAuth (ChatGPT Plus/Pro, Claude Pro/Max, Copilot) is a licensing problem,
   not a technical one.** All are reverse-engineered reuse of first-party `client_id`s and
   **violate the providers' terms**; **Anthropic officially banned it (Feb–Mar 2026) and
   enforces server-side**; OpenAI gates the ChatGPT backend by account claim. Support these
   only as clearly-labelled, opt-in, breakage-prone integrations — never a default.
6. **The clean abstraction is two catalog structs + one auth enum:** `Provider { id, protocol,
   base_url?, auth: Vec<AuthMethod>, extra_headers, query_params }` and `Model { … }`, with
   `AuthMethod = ApiKey | OAuth | CloudIam(AwsSigV4|GcpAdc|AzureEntra) | Acp | None`.
   `auth` is a **`Vec`** because Azure/Bedrock/Vertex/Anthropic each legitimately offer
   several methods on one provider. Make `protocol` **explicit** — every prior-art system
   (models.dev, LiteLLM, Vercel, OpenRouter) leaves it implicit and pays for it.
7. **ACP is a different axis from "model provider."** An ACP agent is an autonomous subprocess
   you *delegate a turn to*; it owns its own model and calls back into you for fs/terminal/
   permission. It slots in as `AuthMethod::Acp { command, args }` + `Protocol::Acp`, not as an
   HTTP endpoint. **The client does not pick the model per turn** — model choice is the agent's,
   optionally exposed via `session/set_config_option`. Rust is ACP's reference language
   (`agent-client-protocol` crate v2.0.0), so a Rust client is first-class.

---

## (a) Direct model-API providers

Wire: `OAI-Chat` = OpenAI Chat Completions wire; `OAI-Resp` = OpenAI Responses API;
`Native` = provider-specific body; `Anthropic` = Anthropic Messages wire.
**Drop-in?** = usable by a generic OpenAI client with config only (base_url + static key).

| Provider | Wire | Base URL | Auth (exact header) | Drop-in? | Key env var | Notes |
|---|---|---|---|---|---|---|
| **OpenAI** | OAI-Chat **+** OAI-Resp | `https://api.openai.com/v1` | `Authorization: Bearer` | YES | `OPENAI_API_KEY` | Responses API now primary; Chat Completions still supported. Codex is deprecating `/chat/completions` — **frontier coding models need OAI-Resp**. |
| **Anthropic (Claude)** | Native Messages **+** OAI-Chat compat | Native `https://api.anthropic.com` (`/v1/messages`); compat `https://api.anthropic.com/v1/` | Native `x-api-key` + `anthropic-version: 2023-06-01`; compat `Authorization: Bearer` | PARTIAL | `ANTHROPIC_API_KEY` | Native not drop-in. Compat `…/v1/` is config-only but **testing-grade** (ignores `response_format`/caching, `n=1`, temp≤1). |
| **Azure OpenAI** (Foundry) | OAI-Chat + OAI-Resp | v1 `https://<res>.openai.azure.com/openai/v1/`; classic `…/openai/deployments/<dep>/chat/completions?api-version=…` | `api-key` **or** Entra `Authorization: Bearer` | PARTIAL | `AZURE_OPENAI_API_KEY` | **New v1 API (GA Aug 2025) drops `api-version`** → near drop-in. `model` = deployment name. |
| **AWS Bedrock** | Native Converse/Invoke **+** OAI-Chat/Resp | compat `https://bedrock-mantle.<region>.api.aws/v1`; native `https://bedrock-runtime.<region>.amazonaws.com` | **SigV4** *or* `Authorization: Bearer <bedrock-key>` | PARTIAL | `AWS_BEARER_TOKEN_BEDROCK` (compat); `AWS_ACCESS_KEY_ID`/`_SECRET_ACCESS_KEY`/`AWS_REGION` (SigV4) | Dec-2025 `bedrock-mantle` speaks OAI-Chat/Resp → config-only **with a Bedrock API key**. Native Converse needs a SigV4 adapter. |
| **Google Vertex AI** | Native `:generateContent` **+** OAI-Chat | compat `https://<LOC>-aiplatform.googleapis.com/v1/projects/<PROJ>/locations/<LOC>/endpoints/openapi` | `Authorization: Bearer <ADC token>` (OAuth2) | PARTIAL | *(OAuth, no static key)* `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`, `GOOGLE_APPLICATION_CREDENTIALS` | Compat path is OAI-shaped **but auth is a short-lived OAuth token** (refresh hook), URL embeds project+location, `model` needs `google/…`/`meta/…` prefix. |
| **Google Gemini API** (AI Studio) | Native **+** OAI-Chat compat | compat `https://generativelanguage.googleapis.com/v1beta/openai/`; native `…/v1beta/models/<m>:generateContent` | compat `Authorization: Bearer`; native `x-goog-api-key` | YES (compat) | `GEMINI_API_KEY` (`GOOGLE_API_KEY`) | Config-only via `…/v1beta/openai/`. `/models`, `/embeddings`, tools, streaming ✓. |
| **Ollama** (local) | Native `/api/chat` **+** OAI-Chat | `http://localhost:11434/v1` | none (any string) | YES | *(n/a)*; `OLLAMA_HOST` | Model must be pulled. `/v1/{models,chat/completions,embeddings,responses}` ✓. |
| **LM Studio** (local) | OAI-Chat (+ OAI-Resp) | `http://localhost:1234/v1` | none | YES | *(n/a)* | `GET /v1/models` lists loaded models. Tools + streaming ✓. |
| **vLLM** (local) | OAI-Chat (+ Completions/Responses) | `http://localhost:8000/v1` | none, or `Authorization: Bearer` if `--api-key` set | YES | server `VLLM_API_KEY`; client `OPENAI_API_KEY`=`EMPTY` | `vllm serve <model>`; one model/process. Tools need `--enable-auto-tool-choice`. |
| **OpenRouter** | OAI-Chat | `https://openrouter.ai/api/v1` | `Authorization: Bearer` | YES | `OPENROUTER_API_KEY` | Pure drop-in aggregator. Namespaced ids (`anthropic/claude-…`). Optional `HTTP-Referer`/`X-OpenRouter-Title`. |
| **Together AI** | OAI-Chat | `https://api.together.ai/v1` | `Authorization: Bearer` | YES | `TOGETHER_API_KEY` | Namespaced ids (`meta-llama/…`). |
| **Groq** | OAI-Chat (+ OAI-Resp) | `https://api.groq.com/openai/v1` | `Authorization: Bearer` | YES | `GROQ_API_KEY` | A few params 400 (`logprobs`, `n>1`); `temp:0`→`1e-8`. |
| **Fireworks AI** | OAI-Chat (+ OAI-Resp beta) | `https://api.fireworks.ai/inference/v1` | `Authorization: Bearer` | YES | `FIREWORKS_API_KEY` | Ids = `accounts/fireworks/models/…`. Streaming usage only in final chunk. |
| **Mistral** (La Plateforme) | OAI-Chat | `https://api.mistral.ai/v1` | `Authorization: Bearer` | YES | `MISTRAL_API_KEY` | Extensions `safe_prompt`, FIM/prefix. |
| **DeepSeek** | OAI-Chat **+** Anthropic | `https://api.deepseek.com` (`/v1` optional); Anthropic `…/anthropic` | `Authorization: Bearer` | YES | `DEEPSEEK_API_KEY` | `deepseek-chat`/`-reasoner` **deprecating 2026-07-24** → aliases of `deepseek-v4-flash`; current `deepseek-v4-flash`/`-pro`. |
| **xAI (Grok)** | OAI-Chat **+** OAI-Resp | `https://api.x.ai/v1` | `Authorization: Bearer` | YES | `XAI_API_KEY` | Chat Completions now "legacy"; steers to Responses. **"Anthropic-compatible" claim could NOT be verified — treat as false.** |
| **Cerebras** | OAI-Chat | `https://api.cerebras.ai/v1` | `Authorization: Bearer` | YES | `CEREBRAS_API_KEY` | `tools`+`response_format` model-dependent. |
| **Nebius Token Factory** (ex-AI Studio) | OAI-Chat | `https://api.tokenfactory.nebius.com/v1/` (legacy `api.studio.nebius.com/v1`) | `Authorization: Bearer` | YES | `NEBIUS_API_KEY` | **Rebranded** AI Studio → Token Factory (301s to new host). |
| **Perplexity (Sonar)** | OAI-Chat (search-grounded) | `https://api.perplexity.ai` (**no `/v1`**) | `Authorization: Bearer` | PARTIAL | `PERPLEXITY_API_KEY` | No `/models`, responses add citations, limited tools. `sonar`, `sonar-pro`, `sonar-reasoning-pro`. |
| **DeepInfra** | OAI-Chat | `https://api.deepinfra.com/v1/openai` | `Authorization: Bearer` | YES | `DEEPINFRA_TOKEN` | Note `/v1/openai` path. Not 100% param-compatible. |
| **Hyperbolic** | OAI-Chat | `https://api.hyperbolic.xyz/v1` | `Authorization: Bearer` | YES | `HYPERBOLIC_API_KEY` | Standard SSE. `/v1/models` expected (unconfirmed this pass). |
| **GitHub Models** | OAI-Chat-shaped (no `/v1`) | `https://models.github.ai/inference` | `Authorization: Bearer <GITHUB_TOKEN>` (PAT `models:read`) | PARTIAL | `GITHUB_TOKEN` | **Old `models.inference.ai.azure.com` removed Oct 2025.** Namespaced ids; strict prototyping limits; `X-GitHub-Api-Version`. |
| **SambaNova Cloud** | OAI-Chat (+ OAI-Resp) | `https://api.sambanova.ai/v1` | `Authorization: Bearer` | PARTIAL | `SAMBANOVA_API_KEY` | Drops penalties/`logit_bias`; `n>1` rejected with tools; adds `top_k`. |
| **Baseten** | OAI-Chat **+** Anthropic (beta) | `https://inference.baseten.co/v1` (Model APIs) | `Authorization: Bearer` | YES | `BASETEN_API_KEY` | Dedicated deployments use per-model hosts (`model-<id>.api.baseten.co`). |
| **Novita AI** | OAI-Chat | `https://api.novita.ai/openai/v1` | `Authorization: Bearer` | YES | `NOVITA_API_KEY` | Current path `/openai/v1` (old `/v3/openai` deprecated). |
| **Lambda** (Inference API) | OAI-Chat | `https://api.lambda.ai/v1` (ex `api.lambdalabs.com`) | `Authorization: Bearer` | YES | `LAMBDA_API_KEY` | Domain moved to `api.lambda.ai`. "Fully OpenAI-compatible." |
| **Featherless AI** | OAI-Chat | `https://api.featherless.ai/v1` | `Authorization: Bearer` | YES | `FEATHERLESS_API_KEY` | Huge catalog via `/v1/models`. |
| **Inference.net** | OAI-Chat | `https://api.inference.net/v1` | `Authorization: Bearer` | YES | `INFERENCE_API_KEY` | Clean drop-in; optional BYO-upstream. |
| **Chutes AI** | OAI-Chat | `https://llm.chutes.ai/v1` | `Authorization: Bearer <cpk_…>` | YES | `CHUTES_API_KEY` | Decentralized/TEE GPUs. X-API-Key not accepted for inference. |
| **Parasail** | OAI-Chat (+ OAI-Resp) | `https://api.parasail.io/v1` | `Authorization: Bearer` | YES | `PARASAIL_API_KEY` | Serverless + dedicated share base. |
| **Venice AI** | OAI-Chat | `https://api.venice.ai/api/v1` | `Authorization: Bearer` | YES | `VENICE_API_KEY` | Doubled `/api/v1`. Privacy-focused; optional `venice_parameters`. |
| **Moonshot AI (Kimi)** | OAI-Chat **+** Anthropic | `https://api.moonshot.ai/v1` (intl) / `.cn/v1`; Anthropic `…/anthropic` | `Authorization: Bearer` | YES | `MOONSHOT_API_KEY` | `.ai`/`.cn` are separate accounts. Kimi K2 family. |
| **Zhipu / Z.ai (GLM)** | OAI-Chat **+** Anthropic | intl `https://api.z.ai/api/paas/v4`; China `https://open.bigmodel.cn/api/paas/v4` | `Authorization: Bearer` | PARTIAL | `ZAI_API_KEY` / `ZHIPUAI_API_KEY` | **Base is `/api/paas/v4`, not `/v1`** (hardcoding `/v1` 404s). GLM-4.6/4.7/5. |
| **MiniMax** | OAI-Chat **+** Anthropic | intl `https://api.minimax.io/v1`; China `https://api.minimaxi.com/v1`; Anthropic `…/anthropic` | `Authorization: Bearer` | YES (`.io/v1`) | `MINIMAX_API_KEY` | China may reject `developer` role. M2/M3 family. |
| **Alibaba Qwen** (DashScope/Model Studio) | OAI-Chat + OAI-Resp + Native | intl `https://dashscope-intl.aliyuncs.com/compatible-mode/v1`; China `https://dashscope.aliyuncs.com/compatible-mode/v1` | `Authorization: Bearer` | YES (compatible-mode) | `DASHSCOPE_API_KEY` | Keys region-scoped; `tools` can't combine with `stream=True` on Qwen. |
| **AI21 (Jamba)** | OAI-Chat-shaped over `/studio/v1` | `https://api.ai21.com/studio/v1` | `Authorization: Bearer` | PARTIAL | `AI21_API_KEY` | Base prefix `/studio/v1` (not `/v1`). `jamba-large`/`-mini`. |
| **Cohere** | Native `/v2/chat` **+** OAI-Chat compat | native `https://api.cohere.com/v2`; compat `https://api.cohere.ai/compatibility/v1` | `Authorization: Bearer` | PARTIAL | `CO_API_KEY` (`COHERE_API_KEY`) | Native `/v2/chat` not OAI-shaped; OAI clients need `…/compatibility/v1`. Top embeddings/rerank. |
| **Voyage AI** (embeddings/rerank) | Native REST (OAI-style embeddings), **no chat** | `https://api.voyageai.com/v1` | `Authorization: Bearer` | PARTIAL (embed only) | `VOYAGE_API_KEY` | `/embeddings` OAI-shaped (+`input_type`). MongoDB-owned. No chat. |
| **Jina AI** (embeddings/rerank/reader) | Native REST; embeddings=OAI-schema, **no chat** | `https://api.jina.ai/v1` | `Authorization: Bearer` | PARTIAL (embed only) | `JINA_API_KEY` | Embeddings match `text-embedding-3-large` I/O. jina-embeddings-v5. |

### Classification: pure-config vs adapter-required

**Pure config (generic OpenAI client, `base_url` + static key, standard `/v1` shape):**
OpenAI · Gemini API (`/v1beta/openai/`) · Ollama · LM Studio · vLLM · OpenRouter · Together ·
Groq · Fireworks · Cerebras · Nebius Token Factory · Baseten (Model APIs) · DeepInfra ·
Hyperbolic · Novita · Lambda · Featherless · Inference.net · Chutes · Parasail · Venice ·
Mistral · DeepSeek · xAI · Moonshot · MiniMax (`.io`) · Qwen (compatible-mode) ·
Anthropic (compat `…/v1/`, testing-grade only).

**Config-only but must tolerate a quirk (a plain "append `/v1` + `/models`" assumption breaks):**
- *Non-standard base path* (accept an arbitrary full base): GitHub Models (`/inference`) ·
  Perplexity (no `/v1`, no `/models`) · Zhipu/Z.ai (`/api/paas/v4`) · AI21 (`/studio/v1`) ·
  Cohere (`/compatibility/v1`) · DeepInfra (`/v1/openai`) · Venice (`/api/v1`) · Groq (`/openai/v1`).
- *Model-id rewriting*: Azure (deployment name) · Vertex/Model Garden (`google/…`,`meta/…`) ·
  Bedrock (Bedrock ids) · namespaced-slug providers (OpenRouter, Together, Fireworks, GitHub Models).
- *Parameter gaps*: SambaNova · Groq · Cerebras.

**Native adapter genuinely required (non-OpenAI body and/or non-string auth):**
- **AWS Bedrock native** Converse/InvokeModel — SigV4 + Bedrock body. (Escape hatch:
  `bedrock-mantle` OpenAI path + API key.)
- **Vertex AI** — even the OpenAI-compat path needs a short-lived OAuth/ADC token + project/
  location in URL. Native `:generateContent` is a separate body.
- **Gemini API native** — `x-goog-api-key` + `:generateContent`. (Escape hatch: `/v1beta/openai/`.)
- **Azure classic** — deployment-in-path + `api-version` + `api-key`. (Escape hatch: `/openai/v1/`.)
- **Anthropic native Messages** — `x-api-key` + `anthropic-version` + Messages body. (Escape hatch: compat `…/v1/`, testing-grade.)
- **Cohere native `/v2/chat`** — Cohere body. (Escape hatch: `/compatibility/v1`.)
- **Voyage / Jina** — embeddings/rerank only; no chat wire → always a dedicated embeddings adapter.

**Design axis:** static-string-key (nearly all) vs **credential-provider-hook** (token refresh
or signing): **AWS Bedrock (SigV4), Vertex AI (OAuth/ADC), Azure (Entra option)**. Those three
justify a pluggable credential provider rather than a `String` key.

---

## (b) OAuth / subscription auth — findings & feasibility caveats

> **Honest bottom line:** every consumer-subscription path below is reverse-engineered reuse of
> a first-party `client_id`/flow and runs against the provider's terms. They break often
> (scope/model gating, header spoofing, active blocking). The only cleanly-supported, durable
> options for a third-party agent are **API keys** and the **cloud-IAM paths (§c)**. Ship
> subscription auth only as an explicit, labelled, opt-in integration with a "may break / may
> violate ToS" warning — never as a default or a headline feature.

### OpenAI "Sign in with ChatGPT" (Plus/Pro) — how Codex CLI does it
- **Flow:** OAuth 2.0 Authorization Code **+ PKCE (S256)**. Codex opens a loopback HTTP server,
  browser → auth server → localhost callback → code→token exchange, proactive refresh (session
  stale after ~8 days). A **device-code flow (beta)** exists for headless/SSH.
- **Concrete values** (reverse-engineered, consistent across official docs + `EvanZhouDev/openai-oauth`):
  authorize `https://auth.openai.com/oauth/authorize`; token `https://auth.openai.com/oauth/token`;
  public native `client_id = app_EMoamEEZ73f0CkXaXp7hrann`; callback `http://localhost:1455/auth/callback`
  (**port 1455**); scopes `openid profile email offline_access`; tokens in `~/.codex/auth.json`.
- **Model backend:** **`https://chatgpt.com/backend-api/codex`** (a Responses-shaped endpoint,
  e.g. `…/codex/responses`) — *not* `api.openai.com`. The JWT carries a `chatgpt_account_id`/plan
  claim the backend requires.
- **Third-party feasibility:** **No supported path.** Architected for OpenAI's own Codex. Reuse
  is technically possible but reverse-engineers report breakage: tokens lack `api.responses.write`
  scope, GPT-5-class calls fail *"Failed to extract accountId from token,"* the auth server rejects
  non-identity scopes. **Unsupported + against ToS.** Sanctioned path = `api.openai.com` + API key.

### Anthropic Claude Pro/Max via Claude Code
- **Flow:** OAuth 2.0 Auth Code + PKCE (S256); browser, with paste-code fallback.
  `claude setup-token` prints a **long-lived (~1yr) OAuth token** for CI.
- **Concrete values** (reverse-engineered; corroborated by the cedws gist):
  `client_id = 9d1c250a-e61b-44d9-88ed-5944d1962f5e`; authorize `https://claude.ai/oauth/authorize`;
  token `https://console.anthropic.com/v1/oauth/token`; scopes `org:create_api_key user:profile
  user:inference`; **required beta header `anthropic-beta: oauth-2025-04-20`**; token `sk-ant-oat01-…`
  in env `CLAUDE_CODE_OAUTH_TOKEN`. Model calls hit `https://api.anthropic.com/v1/messages` with
  `Authorization: Bearer <oauth>` (not `x-api-key`).
- **Third-party legitimacy — NOT PERMITTED (and enforced).** As of **Feb–Mar 2026 Anthropic
  officially banned** using consumer Free/Pro/Max OAuth tokens in **any** third-party tool or
  service (including its own Agent SDK), with **server-side enforcement** (blocking began
  2026-01-09; docs updated 2026-02-19; legal + technical enforcement March 2026). The
  `sk-ant-oat01` token is rejected by the plain Messages API unless a client spoofs Claude Code's
  exact headers/system-prompt. OpenCode's own docs note "Anthropic explicitly prohibits" this.
  **Do not build on it.** Sanctioned path = Console API key (`x-api-key`) or Bedrock/Vertex.

### OpenCode "zen" and `opencode auth login`
- **OpenCode Zen** = OpenCode/SST's own curated **hosted model gateway/marketplace**. Base URL
  **`https://opencode.ai/zen/v1`** (OpenAI-compatible), auth = first-party **Zen API key** from
  `https://opencode.ai/auth`, sent as `x-api-key` (Bearer also works), pay-per-use; a separate
  "Go" plan is a fixed subscription. **This is a clean, supported, drop-in provider** (unlike the
  subscription-passthrough logins) — treat it as an ordinary `ApiKey` provider.
- **`opencode auth login` / TUI `/connect`** store creds in `~/.local/share/opencode/auth.json`
  and support subscription OAuth logins (Anthropic Claude Pro/Max — subject to the ban above;
  GitHub Copilot via device flow). Useful as a *pattern reference* for a credential store.

### GitHub Copilot as a backend (OAuth device flow)
- **Flow:** GitHub **device flow** (`github.com/login/device`, enter code) → user token
  (`gho_`/`ghu_`; classic `ghp_` not accepted). **Two-step exchange:**
  `GET https://api.github.com/copilot_internal/v2/token` with `Authorization: token <ghu_/gho_>`
  → short-lived Copilot bearer + the base URL in `endpoints.api`.
- **Model base:** `https://api.githubcopilot.com` (Business → `api.business.githubcopilot.com`),
  OpenAI-compatible wire. Public VS Code `client_id = Iv1.b507a08c87ecfe98`; requires identity
  headers `Editor-Version`, `Editor-Plugin-Version`, `Copilot-Integration-Id`.
- **Caveat:** intended for Copilot-integrated editors; generic backend use violates Copilot's terms.

### Google Gemini "Sign in with Google" (no API key)
- **Flow:** OAuth installed-app; `client_id = 681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com`;
  local callback; creds in `~/.gemini/oauth_creds.json`. Backend = **Code Assist**
  (`https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` → tier detect →
  `:generateContent`), not the API-key endpoint.
- **2026 caveat (least-stable item):** on/around **2026-06-18 Google discontinued the free
  "Sign in with Google" path (and AI Pro/Ultra login) in Gemini CLI**, redirecting unpaid/Google-One
  users to **Antigravity CLI**; a paid **Gemini API key** still works. Verify before depending on
  the free OAuth path.

---

## (c) Cloud-IAM requirements per cloud

### Azure OpenAI (Microsoft Foundry)
- **Two auth modes:** (a) resource key `api-key: <key>`; (b) **Entra ID** `Authorization: Bearer
  <token>`, token **scope `https://cognitiveservices.azure.com/.default`** (unified Foundry also
  accepts `https://ai.azure.com/.default`), acquired via `DefaultAzureCredential`/MSAL; identity
  needs RBAC role **Cognitive Services OpenAI User**.
- **Endpoint shapes:** classic `…/openai/deployments/<dep>/chat/completions?api-version=YYYY-MM-DD`;
  **new v1 (GA ~Aug 2025) `…/openai/v1/…`** incl. `/openai/v1/responses` — **drops the `api-version`
  query** (GA needs none; preview uses `api-version=preview`), so the stock OpenAI SDK points at
  `.../openai/v1/`.
- **Client must implement:** Entra token acquire + refresh (~60–90 min) *or* the static `api-key`;
  URL assembly (deployment + api-version for classic, or `/openai/v1/`); SSE streaming.

### AWS Bedrock
- **APIs:** `bedrock-runtime` **InvokeModel** / **InvokeModelWithResponseStream** (model-native
  bodies) and the unified **Converse** / **ConverseStream**. Paths `POST /model/{modelId}/{invoke|
  invoke-with-response-stream|converse|converse-stream}` on host
  `bedrock-runtime.{region}.amazonaws.com`. Model ids e.g. `anthropic.claude-3-5-sonnet-…` or
  cross-region inference profiles `us.anthropic.claude-sonnet-4-6`.
- **Auth mode 1 — SigV4 (primary):** AWS Signature v4 signing, **service name `bedrock`**, region
  from host, IAM access-key/secret (+ optional session token) or assumed role. Needs
  `bedrock:InvokeModel`.
- **Auth mode 2 — Bedrock API key / bearer (added 2025, live):** `Authorization: Bearer <key>` /
  env `AWS_BEARER_TOKEN_BEDROCK`. **Short-term** (≤12 h, minted from an STS session via
  `aws-bedrock-token-generator`, inherits principal perms — recommended) or **long-term** (IAM
  service-specific credential, exploration only). Gated by IAM action `bedrock:CallWithBearerToken`.
  Plus the Dec-2025 **`bedrock-mantle.<region>.api.aws/v1`** OpenAI-compatible surface.
- **Client must implement:** either a **SigV4 signer** (canonical request, `bedrock` service,
  region, SHA256 payload hash, `X-Amz-*` headers) **or** just attach `Authorization: Bearer` with
  an API key; region-in-host URL building; model/inference-profile ids; AWS event-stream parsing
  for `*-stream`. **The bearer key + mantle endpoint is the easy path; SigV4 is the hard path.**

### GCP Vertex AI
- **Auth:** Application Default Credentials / service-account JSON → **OAuth2 access token** →
  `Authorization: Bearer <token>`, **scope `https://www.googleapis.com/auth/cloud-platform`**.
  Token ~1 h → must refresh (mint locally from SA JWT, or GCE/GKE metadata server on-cluster).
  Identity needs role **Vertex AI User**.
- **Endpoint shapes** (`{region}` e.g. `us-central1`, or global host + `locations/global`):
  native Gemini `…/publishers/google/models/{model}:generateContent` (stream `:streamGenerateContent`);
  **Anthropic Claude on Vertex** `…/publishers/anthropic/models/{model}:rawPredict`
  (stream `:streamRawPredict`) with body field `"anthropic_version": "vertex-2023-10-16"` and
  **no top-level `model`**; **OpenAI-compatible** `…/endpoints/openapi/chat/completions`.
- **Client must implement:** google-auth token minting from SA JSON (signed JWT → token exchange)
  or metadata-server fetch; refresh before ~1 h expiry; URL assembly (project/region/publisher/
  model + verb suffix); `Authorization: Bearer`; SSE. **No static key** — this is the hardest
  *unavoidable* auth (Vertex has no bearer-key shortcut like Bedrock's).

---

## (d) ACP (Agent Client Protocol) — how it works, who speaks it, how to connect

> **"Model provider" ≠ "ACP agent."** A model provider is something *you call* (send prompts, get
> tokens; you own the agent loop — the MCP/LLM-SDK layer). An **ACP agent** is an autonomous
> coding-agent **subprocess you delegate a turn to**: it runs its own loop, picks its **own**
> model, does its **own** tool calls, and **calls back into you** to read/write files, run
> terminals, and ask permission, streaming `session/update` until it returns a `stopReason`. As an
> ACP *client* you are the editor/host — you never see raw model tokens or drive the loop.
>
> Naming landmine: this is **Zed's Agent *Client* Protocol** (agentclientprotocol.com), *not*
> IBM's Agent *Communication* Protocol (which merged into Google's A2A).

### How it works
- **Transport:** **JSON-RPC 2.0 over the agent subprocess's stdio** (client writes stdin, reads
  stdout; stderr = agent logs). **Newline-delimited JSON** (one message/line, `\n`) — **no**
  LSP-style `Content-Length` headers. A remote/HTTP transport also exists but stdio-subprocess is
  the default for a coding client.
- **Wire `protocolVersion` is an integer, currently `1`** (`ProtocolVersion::V1`) — distinct from
  library semver (Rust crate is 2.0.0).
- **Methods the CLIENT calls (agent implements):** `initialize`, `authenticate`, `logout`,
  `session/new`, `session/load`, `session/resume|fork|list|delete|close`, `session/prompt`,
  `session/set_mode`, **`session/set_config_option`** (where model selection lives),
  `providers/list|set|disable`. Client→agent notification: `session/cancel`.
- **Methods the AGENT calls back (client implements):** `session/request_permission`,
  `fs/read_text_file`, `fs/write_text_file`, `terminal/create|output|wait_for_exit|kill|release`,
  `elicitation/create` (feature-gated). Agent→client notification: **`session/update`** — the
  streaming firehose (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`,
  `plan`, `available_commands_update`, `current_mode_update`, `usage_update`).
- **Handshake / lifecycle:** `initialize` (negotiate `protocolVersion` + exchange
  `clientCapabilities`{fs, terminal} / `agentCapabilities`{loadSession, promptCapabilities,
  mcpCapabilities} + `authMethods[]`) → `authenticate` if `authMethods` non-empty (many adapters
  instead expect the underlying CLI already logged in, or read `OPENAI_API_KEY`/`CURSOR_API_KEY`)
  → `session/new` (pass cwd + MCP servers to expose) returns `sessionId` → `session/prompt`
  `{sessionId, prompt:[ContentBlock…]}` (long-lived; streams updates + callbacks; resolves with a
  `stopReason`: `end_turn|max_tokens|max_turn_requests|refusal|cancelled`) → `session/cancel` to abort.

### Model-selection delegation (crucial)
**The agent owns the model; the client does NOT send a model id in `session/prompt`.**
- **Modes ≠ models:** `session/set_mode` switches behavioral modes (ask/architect/code —
  `SessionMode{id,name,description}`), changing prompt/tools/permissions, **not** the LLM.
- **Model selection = a session config option:** ACP generalized it into
  **`session/set_config_option`** (`configId`+`value`) with a well-known config-option kind
  `Model`. *If the agent exposes it*, it advertises available models in `configOptions` and the
  client sets one. Supersedes the earlier experimental `unstable_setSessionModel`. Still settling;
  exposure is **per-agent-optional**.
- **Provider config:** `providers/list|set|disable` let a client inspect/configure the agent's
  *LLM providers* (`ProviderInfo{provider_id, protocol, base_url, mandatory}`) — the "point the
  agent at OpenAI vs a custom gateway" knob, not a per-prompt model id.
- **For a client:** treat model choice as a **capability you probe** (read `configOptions`,
  optionally `providers/list`; drive with `session/set_config_option`/`providers/set`), never a
  parameter you assume. If neither is exposed, the model is entirely the agent's business.

### Which agents speak ACP (July 2026) and how a client connects

The ecosystem moved from `zed-industries/*` into a community-governed **`agentclientprotocol`**
org + **`@agentclientprotocol/*`** npm scope (Zed still primary steward). Old names redirect.

| Agent product | ACP-capable? | Native/adapter | How a client launches/connects |
|---|---|---|---|
| **Gemini CLI** (Google) | Yes | **Native** | `gemini --acp` (older `--experimental-acp` still works) |
| **Claude Code / Claude Agent** | Yes | **Adapter** `@agentclientprotocol/claude-agent-acp` (wraps Claude Agent SDK; formerly `@zed-industries/claude-code-acp`) | `npx @agentclientprotocol/claude-agent-acp` (bin `claude-agent-acp`); expects Claude CLI authenticated |
| **OpenAI Codex** | Yes | **Adapter** `@agentclientprotocol/codex-acp` (bundles `@openai/codex`) | `npx -y @agentclientprotocol/codex-acp`; auth via `OPENAI_API_KEY`/`CODEX_API_KEY` |
| **OpenCode** (SST) | Yes | **Native** | `opencode acp` → `{command:"opencode", args:["acp"]}` |
| **Cursor CLI** (`cursor-agent`) | Yes | **Native** (binary `agent`) | `agent acp` (e.g. `agent --api-key "$CURSOR_API_KEY" acp`); also over ACP in JetBrains |
| **Google Antigravity** (`agy`) | Own surfaces; **native ACP pending** | Community adapter for now | via `shubzkothekar/antigravity-acp` / `acpx agy --acp stdio`; native `--acp` requested (issue #31) but not shipped |
| **Zed agent** | Yes (Zed is the reference **client**) | Native | in-process; external agents via `agent_servers` config |
| **Goose** (Block) | Yes — primarily an ACP **client/host** | Client launches adapters | `export GOOSE_PROVIDER=claude-acp\|codex-acp\|amp-acp\|pi-acp` |
| **Amp** (spun out of Sourcegraph) | Yes | **Adapter** `amp-acp` | `npx amp-acp` |
| **Aider** | Partial — ACP **orchestrator/client** (dispatches to Claude Code/Codex/Cursor) | Mixed | acts as client; "Aider as ACP *server*" unverified |
| Kilo Code, Kimi CLI, Qwen Code, JetBrains **Junie**, Cline, OpenHands, GitHub Copilot CLI, Mistral Vibe, Grok, Docker cagent, Factory Droid, Pi, Kiro CLI | Listed in Zed's 50+ registry | Mostly native/thin adapters (Qwen Code is a Gemini-CLI fork → `--acp`) | per-agent; confirm exact subcommand in the Zed ACP registry |

**ACP *client*/host products** (the category Codypendent would join): Zed (native), **JetBrains**
IDEs (native AI Assistant ACP), Qt Creator, Unity; Neovim (CodeCompanion, avante.nvim,
agentic.nvim); Emacs (`agent-shell`); VS Code extensions (`vscode-acp` — MS has no native ACP);
Obsidian; Marimo, Jupyter (`agent-client-kernel`), DuckDB; orchestrators **Goose** and
`openclaw/acpx`.

### ACP vs MCP vs A2A
- **MCP** (Anthropic) = agent↔tools/context layer; **embedded in ACP** (`session/new` hands the
  agent MCP servers; ACP reuses MCP content-block/elicitation types). MCP gives an agent its tools.
- **ACP** (Zed) = host/editor↔autonomous-agent layer over stdio JSON-RPC (sessions, streaming,
  permissions, client fs/terminal). ACP plugs that agent into an editor. Sits *above* MCP.
- **A2A** (Google→Linux Foundation) = agent↔agent peer delegation over HTTP (AgentCard/AgentSkill).
  A2A lets agents delegate to each other. (IBM's *Agent Communication Protocol* — a different "ACP"
  — merged into A2A; unrelated to Zed's ACP.)

### Rust support (good news)
- **Official Rust crate `agent-client-protocol` v2.0.0** (repo `agentclientprotocol/rust-sdk`) —
  **Rust is the reference implementation**; wire types in `agent-client-protocol-schema` v1.6.0
  (`schema::v1::*`, `ProtocolVersion::V1`). Implements **both** sides, tokio-based, builder API.
  Canonical client example `examples/yolo_one_shot_client.rs`: spawn the agent from a command
  string (`"gemini --acp"`, `"npx @agentclientprotocol/codex-acp"`), register handlers for the
  agent's callbacks (`session/request_permission`, `fs/*`, `terminal/*`), `connect_with`, then
  `initialize` → `session/new` → `session/prompt` and consume streamed `SessionNotification`s.
- Other SDKs (for the agents, not needed by a Rust client): TS `@agentclientprotocol/sdk`,
  Python `agent-client-protocol`, Kotlin/Java, community Go.

---

## (e) Recommended minimal provider / auth data model

Two catalog structs + one auth enum. Preserves Codypendent's invariants: **secrets are
referenced by env-var NAME (or fetched from an OS keychain / minted per call), never stored in the
config or logged**; the config is 100 % safe to commit. `auth` is a **`Vec`** because
Azure/Bedrock/Vertex/Anthropic legitimately offer several methods on one provider — the load-bearing
decision that lets one Anthropic entry offer "paste a key **or** log in with a subscription."

```rust
/// One callable endpoint family. (provider_id, model_id) is the addressing scheme (à la
/// Vercel `openai:gpt-4o`, OpenRouter `anthropic/claude-…`).
pub struct Provider {
    pub id: String,                    // "openai", "anthropic", "amazon-bedrock"
    pub name: String,
    pub protocol: Protocol,            // THE wire format — explicit (all prior art leaves it implicit)
    pub base_url: Option<String>,      // models.dev `api`; None => protocol default
    pub auth: Vec<AuthMethod>,         // 1+ supported methods, in preference order
    pub extra_headers: BTreeMap<String, String>, // static, e.g. {"anthropic-version":"2023-06-01"}
    pub query_params:  BTreeMap<String, String>, // static, e.g. Azure classic {"api-version":"…"}
    pub doc: Option<String>,
    pub sdk_ref: Option<String>,       // provenance only (models.dev `npm`)
}

/// The half none of the catalogs name explicitly.
pub enum Protocol {
    OpenAiChat,          // POST {base}/chat/completions   (today's only case)
    OpenAiResponses,     // POST {base}/responses          (OpenAI/xAI/Azure frontier)
    AnthropicMessages,   // POST {base}/v1/messages        (+ anthropic-version header)
    GoogleGenerative,    // Gemini / Vertex :generateContent
    BedrockConverse,     // AWS Converse (SigV4-signed body)
    Acp,                 // NOT http: spawned subprocess, JSON-RPC 2.0 over stdio
}

pub enum AuthMethod {
    ApiKey(ApiKeyAuth),
    OAuth(OAuthAuth),          // subscription (ChatGPT/Claude/Copilot) — opt-in, ToS-gated
    CloudIam(CloudIam),
    Acp(AcpLaunch),
    None,                      // local: ollama / lmstudio / vllm
}

pub struct ApiKeyAuth {
    pub env: Vec<String>,          // ordered env-var NAMEs, first set wins (models.dev `env`)
    pub header: String,            // "Authorization" | "x-api-key" | "api-key"
    pub prefix: String,            // "Bearer " | ""   (Anthropic native uses "")
    pub query_param: Option<String>, // some providers accept the key in the URL
}

pub struct OAuthAuth {             // Claude Pro/Max, ChatGPT, Copilot, Gemini-login
    pub authorize_url: String, pub token_url: String,
    pub client_id: String, pub scopes: Vec<String>,
    pub redirect_uri: String,      // loopback for PKCE, e.g. http://localhost:1455/auth/callback
    pub pkce: bool,
    pub header: String, pub prefix: String,             // how the access token is injected
    pub extra_headers: BTreeMap<String, String>,        // e.g. {"anthropic-beta":"oauth-2025-04-20"}
    pub token_exchange_url: Option<String>,             // two-step: identity token -> inference token (Copilot/Gemini)
    pub refresh: bool,
}

pub enum CloudIam {
    AwsSigV4 {                     // Bedrock native
        service: String,           // "bedrock"
        region_env: String,        // "AWS_REGION"
        access_key_env: String, secret_key_env: String, session_token_env: Option<String>,
        bearer_token_env: Option<String>, // "AWS_BEARER_TOKEN_BEDROCK" — the key-like shortcut
    },
    GcpAdc {                       // Vertex
        project_env: String,       // "GOOGLE_CLOUD_PROJECT"
        location_env: String,      // "GOOGLE_CLOUD_LOCATION"
        credentials_env: String,   // "GOOGLE_APPLICATION_CREDENTIALS" (SA json / ADC)
        scopes: Vec<String>,       // ["https://www.googleapis.com/auth/cloud-platform"]
    },
    AzureEntra {                   // Azure key OR AAD
        resource_env: String,      // "AZURE_RESOURCE_NAME"
        api_version: Option<String>,
        scopes: Vec<String>,       // ["https://cognitiveservices.azure.com/.default"]
    },
}

pub struct AcpLaunch {             // delegate a turn to an autonomous agent subprocess
    pub command: String,           // "gemini" | "opencode" | "npx"
    pub args: Vec<String>,         // ["--acp"] | ["acp"] | ["-y","@agentclientprotocol/codex-acp"]
    pub env: BTreeMap<String, String>,
    pub protocol_version: Option<u32>, // 1
}

/// Catalog row, keyed by (provider_id, id). Union of models.dev / LiteLLM / OpenRouter fields.
pub struct Model {
    pub id: String, pub provider_id: String, pub name: String, pub family: Option<String>,
    pub tool_call: bool, pub reasoning: bool, pub structured_output: bool,
    pub attachment: bool, pub temperature: bool,
    pub modalities: Modalities,    // {input, output} of Text|Image|Pdf|Audio|Video
    pub limit: Limit,              // {context, output}
    pub cost: Cost,                // normalized USD / 1M tokens
    pub knowledge: Option<String>, pub release_date: Option<String>, pub last_updated: Option<String>,
    pub open_weights: bool, pub status: Option<String>, // "deprecated"
}
pub struct Modalities { pub input: Vec<Modality>, pub output: Vec<Modality> }
pub struct Limit { pub context: u64, pub output: u64 }
pub struct Cost { pub input: f64, pub output: f64,
                  pub cache_read: Option<f64>, pub cache_write: Option<f64>,
                  pub reasoning_output: Option<f64> } // USD per 1M tokens
```

### Equivalent `models.toml` (backward-compatible superset of today's `[[model]]`)

```toml
# Today's entry still parses: protocol defaults to "openai-chat", auth to a single api_key.
[[provider]]
id = "openai"
protocol = "openai-chat"          # or "openai-responses" for frontier coding models
base_url = "https://api.openai.com/v1"
  [[provider.auth]]
  kind = "api_key"; env = ["OPENAI_API_KEY"]; header = "Authorization"; prefix = "Bearer "

[[provider]]
id = "anthropic"
protocol = "anthropic-messages"
extra_headers = { "anthropic-version" = "2023-06-01" }
  [[provider.auth]]                # paste an API key …
  kind = "api_key"; env = ["ANTHROPIC_API_KEY"]; header = "x-api-key"; prefix = ""
  [[provider.auth]]                # … OR subscription OAuth (opt-in; ToS-gated; see §b)
  kind = "oauth"; client_id = "9d1c250a-…"
  authorize_url = "https://claude.ai/oauth/authorize"
  token_url = "https://console.anthropic.com/v1/oauth/token"
  pkce = true; header = "Authorization"; prefix = "Bearer "
  extra_headers = { "anthropic-beta" = "oauth-2025-04-20" }

[[provider]]
id = "amazon-bedrock"; protocol = "openai-chat"           # via the bedrock-mantle escape hatch
base_url = "https://bedrock-mantle.us-east-1.api.aws/v1"
  [[provider.auth]]                # easy path: bearer key, no SigV4
  kind = "api_key"; env = ["AWS_BEARER_TOKEN_BEDROCK"]; header = "Authorization"; prefix = "Bearer "
  [[provider.auth]]                # hard path: SigV4 against bedrock-runtime + protocol="bedrock-converse"
  kind = "cloud_iam"; variant = "aws_sigv4"; service = "bedrock"
  region_env = "AWS_REGION"; access_key_env = "AWS_ACCESS_KEY_ID"; secret_key_env = "AWS_SECRET_ACCESS_KEY"

[[provider]]
id = "vertex"; protocol = "openai-chat"
base_url = "https://us-central1-aiplatform.googleapis.com/v1/projects/PROJ/locations/us-central1/endpoints/openapi"
  [[provider.auth]]
  kind = "cloud_iam"; variant = "gcp_adc"
  project_env = "GOOGLE_CLOUD_PROJECT"; location_env = "GOOGLE_CLOUD_LOCATION"
  credentials_env = "GOOGLE_APPLICATION_CREDENTIALS"
  scopes = ["https://www.googleapis.com/auth/cloud-platform"]

[[provider]]
id = "ollama"; protocol = "openai-chat"; base_url = "http://localhost:11434/v1"
  [[provider.auth]]
  kind = "none"

[[provider]]                        # an ACP agent, not an HTTP endpoint
id = "gemini-cli-acp"; protocol = "acp"
  [[provider.auth]]
  kind = "acp"; command = "gemini"; args = ["--acp"]
```

### How the four prior-art systems map on

| Concept | models.dev | LiteLLM | Vercel AI SDK | OpenRouter | → this model |
|---|---|---|---|---|---|
| provider id | folder / json key | `custom_llm_provider` | factory instance | vendor part of slug | `Provider.id` |
| model id | model TOML filename | model part of `provider/model` | `languageModel(id)` | `{vendor}/{slug}` | `Model.id` |
| addressing | provider+model | `"provider/model"` string | `registry.languageModel("p:id")` | `vendor/slug` | `(provider_id, id)` |
| wire protocol | implicit in `npm` | `litellm_provider` | implicit in package | fixed OAI-chat | **`Protocol`** (explicit) |
| base URL | `api` (optional) | `api_base` | `baseURL` | fixed | `base_url` |
| api-key auth | `env` list + implied header | `*_API_KEY` / `api_key` | factory `apiKey` | `OPENROUTER_API_KEY` | `AuthMethod::ApiKey` |
| cloud IAM | `env` list (AWS_/GOOGLE_/AZURE_) | `aws_*`/`vertex_*`/`azure_ad_token` | `credentialProvider`/`googleAuthOptions` | n/a | `AuthMethod::CloudIam` |
| subscription OAuth | separate id + `GITHUB_TOKEN` | n/a | n/a | n/a | `AuthMethod::OAuth` |
| ACP subprocess | n/a | n/a | n/a | n/a | `AuthMethod::Acp` |
| ctx/output limit | `limit{context,output}` | `max_input_tokens`/`max_output_tokens` | — | `context_length`/`top_provider.max_completion_tokens` | `Limit` |
| pricing | `cost` USD/1M | `input_cost_per_token` USD/tok | — | `pricing.prompt` USD-str/tok | `Cost` (norm /1M) |
| capabilities | `tool_call`/`reasoning`/`attachment` | `supports_function_calling`/… | — | `supported_parameters[]` | `Model.*` bools |

**Minimal fields per operation:**
- (a) **HTTP direct provider:** `Protocol` + `base_url` (or default) + `ApiKeyAuth{env,header,prefix}` + `Model.id`.
- (b) **OAuth/subscription:** `OAuthAuth{authorize_url,token_url,client_id,scopes,redirect_uri,pkce}` to get the token, then `{header,prefix,extra_headers}` to inject it (+ `token_exchange_url` for Copilot/Gemini).
- (c) **Cloud-IAM signed:** the `CloudIam` variant supplies env-var NAMEs for creds + region/project/resource; the signer (SigV4 / ADC token exchange / Entra `DefaultAzureCredential`) and base_url derive from those. No static key stored.
- (d) **ACP agent:** `AcpLaunch{command,args,env}` — spawn, speak JSON-RPC 2.0 over stdio; the model is negotiated inside the session, not chosen from the catalog.

**Providers needing multiple `AuthMethod`s:** Anthropic `[ApiKey, OAuth]` · Azure `[ApiKey,
CloudIam(AzureEntra)]` · Bedrock `[ApiKey(bearer), CloudIam(AwsSigV4)]` · Vertex `[CloudIam(GcpAdc)]`
· Copilot `[OAuth]` · OpenAI/OpenRouter/Groq/xAI `[ApiKey]` · Ollama/LM Studio/vLLM `[None]` ·
Claude Code / Gemini CLI (as agents) `[Acp]`.

---

## (f) Full recommended provider / agent list for a 2026 power user

**Tier 1 — frontier direct APIs (key):** OpenAI, Anthropic, Google Gemini API, xAI (Grok),
DeepSeek, Mistral, Moonshot/Kimi, Zhipu/Z.ai GLM, Qwen/DashScope.

**Tier 1 — cloud enterprise (IAM):** Azure OpenAI (key/Entra), AWS Bedrock (bearer/SigV4),
GCP Vertex AI (ADC) — each also fronts Anthropic/Meta/etc.

**Tier 2 — inference aggregators / fast hosts (key, drop-in):** OpenRouter, Together, Groq,
Fireworks, Cerebras, SambaNova, DeepInfra, Novita, Nebius Token Factory, Hyperbolic, Baseten,
Lambda, Featherless, Inference.net, Parasail, Perplexity (Sonar), **OpenCode Zen**, MiniMax,
Chutes, Venice, plus **GitHub Models** (Bearer `GITHUB_TOKEN`).

**Tier 2 — local (no auth):** Ollama, LM Studio, vLLM (+ llama.cpp `llama-server`, LocalAI,
Jan, KoboldCpp, Text-Generation-WebUI — all OpenAI-compatible servers, same `[None]` shape).

**Tier 3 — embeddings/rerank (dedicated adapter, no chat):** Voyage, Jina, Cohere (also chat via
`/compatibility/v1`), OpenAI/Gemini embeddings.

**ACP agents (delegate a turn; agent owns its model):** Gemini CLI (`gemini --acp`), Claude Code
(`npx @agentclientprotocol/claude-agent-acp`), OpenAI Codex (`npx @agentclientprotocol/codex-acp`),
OpenCode (`opencode acp`), Cursor CLI (`agent acp`), Amp (`amp-acp`), Goose (host), Antigravity
(adapter, native pending), plus the long tail (Qwen Code, Kimi CLI, Junie, Cline, OpenHands, Copilot
CLI, Kilo, Factory Droid, Pi) via the Zed ACP registry.

**Subscription OAuth (opt-in, ToS-gated, breakage-prone — label loudly):** ChatGPT Plus/Pro (Codex
backend), Claude Pro/Max (**banned by Anthropic Feb–Mar 2026 — do not build on**), GitHub Copilot
(device flow), Gemini free login (discontinued 2026-06-18 → Antigravity).

---

## Sources

**ACP:** https://agentclientprotocol.com/protocol/overview · /protocol/initialization ·
/protocol/session-setup · /protocol/prompt-turn · /protocol/session-modes ·
/rfds/session-config-options · /overview/clients · /libraries/rust ·
https://github.com/agentclientprotocol/agent-client-protocol ·
https://github.com/agentclientprotocol/rust-sdk · https://crates.io/crates/agent-client-protocol
(v2.0.0) · https://crates.io/crates/agent-client-protocol-schema (v1.6.0) ·
https://geminicli.com/docs/cli/acp-mode/ · https://opencode.ai/docs/acp/ ·
https://cursor.com/docs/cli/acp · https://www.npmjs.com/package/@agentclientprotocol/claude-agent-acp
· https://www.npmjs.com/package/@agentclientprotocol/codex-acp · https://zed.dev/acp ·
https://goose-docs.ai/docs/guides/acp-providers/ ·
https://github.com/google-antigravity/antigravity-cli/issues/31 ·
https://tyk.io/learning-center/agent-protocols-a-complete-guide-to-mcp-a2a-and-acp/

**OAuth / subscription:** https://learn.chatgpt.com/docs/auth ·
https://github.com/EvanZhouDev/openai-oauth · https://github.com/openai/codex/issues/8112 ·
https://code.claude.com/docs/en/authentication ·
https://gist.github.com/cedws/3a24b2c7569bb610e24aa90dd217d9f2 ·
https://winbuzzer.com/2026/02/19/anthropic-bans-claude-subscription-oauth-in-third-party-apps-xcxwbn/
· https://github.com/anthropics/claude-code/issues/28091 ·
https://opencode.ai/docs/providers/ · https://opencode.ai/docs/zen/ ·
https://docs.litellm.ai/docs/providers/github_copilot ·
https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/authenticate-copilot-cli ·
https://github.com/google-gemini/gemini-cli/blob/main/docs/get-started/authentication.mdx

**Cloud IAM:** https://learn.microsoft.com/en-us/azure/foundry/openai/api-version-lifecycle ·
https://learn.microsoft.com/en-us/azure/foundry/foundry-models/how-to/configure-entra-id ·
https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html ·
https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys.html ·
https://docs.aws.amazon.com/bedrock/latest/userguide/inference-chat-completions-mantle.html ·
https://www.wiz.io/blog/a-new-type-of-long-lived-key-on-aws-bedrock-api-keys ·
https://docs.cloud.google.com/vertex-ai/generative-ai/docs/migrate/openai/auth-and-credentials ·
https://docs.cloud.google.com/vertex-ai/generative-ai/docs/start/openai ·
https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/claude/use-claude

**Prior art / data model:** https://models.dev/api.json · https://github.com/sst/models.dev
(provider/model TOMLs) · https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json
· https://docs.litellm.ai/docs/providers · https://ai-sdk.dev/docs/ai-sdk-core/provider-management ·
https://ai-sdk.dev/providers/ai-sdk-providers/openai-compatible · https://openrouter.ai/api/v1/models
· https://openrouter.ai/docs/features/provider-routing

**Direct providers** (representative): https://developers.openai.com/api/reference/overview ·
https://platform.claude.com/docs/en/api/openai-sdk · https://ai.google.dev/gemini-api/docs/openai ·
https://docs.ollama.com/api/openai-compatibility · https://lmstudio.ai/docs/developer/openai-compat ·
https://docs.vllm.ai/en/stable/serving/online_serving/ · https://openrouter.ai/docs/quickstart ·
https://docs.github.com/en/rest/models/inference · https://docs.together.ai/docs/openai-api-compatibility
· https://console.groq.com/docs/openai · https://docs.fireworks.ai/tools-sdks/openai-compatibility ·
https://inference-docs.cerebras.ai/resources/openai · https://docs.sambanova.ai/docs/en/features/openai-compatibility
· https://docs.tokenfactory.nebius.com/api-reference/introduction · https://docs.baseten.co/inference/model-apis/overview
· https://docs.deepinfra.com/chat/overview · https://novita.ai/docs/guides/llm-api ·
https://docs.lambda.ai/public-cloud/lambda-inference-api/ · https://docs.mistral.ai/api/ ·
https://api-docs.deepseek.com/ · https://docs.x.ai/docs/api-reference · https://docs.perplexity.ai/docs/sonar/openai-compatibility
· https://platform.kimi.ai/docs/api/chat · https://docs.z.ai/api-reference/introduction ·
https://platform.minimax.io/docs/api-reference/text-openai-api ·
https://www.alibabacloud.com/help/en/model-studio/compatibility-of-openai-with-dashscope ·
https://docs.cohere.com/docs/compatibility-api · https://docs.voyageai.com/reference/embeddings-api ·
https://jina.ai/embeddings/

**Confidence caveats:** xAI "Anthropic-compatible" on hosted `api.x.ai` — **could not be verified,
treat as false**; MiniMax China `api.minimaxi.com/v1` and Hyperbolic `/v1/models` — multi-source but
not single-official-fetch; Gemini free-login discontinuation and Anthropic OAuth ban — fast-moving,
re-verify before shipping. SEO-spam/fabricated repos surfaced during research were disregarded.
