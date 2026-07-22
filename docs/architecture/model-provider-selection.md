# Model & Provider Selection

Design note for Codypendent's TUI model/provider picker and provider
authentication. Status: **proposed** (direction, not yet implemented). Relates to
[`docs/docs/09-model-routing-and-compaction.md`](../docs/09-model-routing-and-compaction.md),
[`docs/docs/19-competitive-design-synthesis.md`](../docs/19-competitive-design-synthesis.md),
and [`docs/docs/20-interaction-and-autonomy-model.md`](../docs/20-interaction-and-autonomy-model.md).

## Purpose

Codypendent already has the *engine* for model choice — a versioned
`RoutingPolicy`, measured `ModelProfile`s (`codypendent models bench`), and a
security/privacy hard-filter that refuses to route classified data off-device.
What it lacks is the *client surface*: a way for a person at the TUI to see, pick,
and connect providers and models. This note synthesises the two best terminal
implementations surveyed in July 2026 — OpenAI **Codex** (Rust + ratatui) and
**OpenCode** (its v2 TypeScript/SolidJS + OpenTUI rewrite) — into a
Codypendent-native design, and names the one thing neither has that we can add:
**classification-aware selection**.

Because Codex is the same stack we are (Rust + ratatui), its widgets are the
implementation substrate; OpenCode contributes the multi-provider data model.

## What to take from each

**From OpenCode** (`packages/tui/src/component/dialog-model.tsx`, `dialog-provider.tsx`):

- Catalog-as-**data** with rich per-model metadata (cost, context window,
  capabilities, `status`), **multi-provider**.
- A **Favorites → Recent → grouped-by-provider** list with **fuzzy search over
  (model + provider)**.
- **Data-driven auth methods** per provider (`type: api | oauth` plus declarative
  `prompts`), so adding a subscription is *data*, not a code path; a provider with
  more than one method pops a "select auth method" step.
- **Connect → jump straight into that provider's model picker**; a green `✓`
  gutter on connected providers.

**From Codex** (`codex-rs/tui/src/chatwidget/model_popups.rs`, `onboarding/auth.rs`):

- A **generic `SelectionItem` / `show_selection_view` popup** — event-driven
  (each item's action is a closure that emits an `AppEvent`), with
  `is_current`/`is_default`, `dismiss_on_select`, `dismiss_parent_on_child_accept`.
  One reusable widget backs model, reasoning, and scope pickers.
- A **two-step gate for expensive/risky choices** (their Max/Ultra "More
  reasoning…" step) so a costly option cannot be selected by a single keystroke.
- A **warning shown only on the highlighted row** (`selected_description`).
- An **auth state-machine** (`SignInState`): OAuth-in-browser **plus device-code
  for headless/remote**, plus API-key entry with **env-var prefill**, a
  **forced-login-method** policy for locked-down installs, and **apply-vs-persist**
  separation (change the live session vs write config).
- **OSC-8 clickable auth URLs with control-character sanitisation** (strips
  ESC/BEL so a crafted URL cannot inject an escape sequence).
- **Hidden-model filtering** (`show_in_picker`) with a `-m <model>` power-user
  escape hatch; **snapshot-tested** popups.

## Codypendent-native design

### 1. Catalog

A `ModelPreset`-shaped record (à la Codex) per selectable model, but populated
from three sources layered together:

- **Static metadata** — provider, display name, cost, context window,
  capabilities (an OpenCode/`models.dev`-style catalog gives this breadth for
  free).
- **Measured profile** — reliability, per-task-class success, latency, and real
  cost from `ModelProfile` (`codypendent models bench`). This is our edge over a
  purely static catalog: the picker can *rank by measured utility*, not vendor
  claims.
- **Classification eligibility** — each model carries a `ModelLocation`
  (`Local` / `Hosted`); the picker resolves it against the run's
  `DataClassification` and the active `RoutingPolicy`'s off-device ceiling.

A `show_in_picker` predicate hides deprecated/experimental models, with a
`codypendent … -m <provider/model>` escape hatch for power users (Codex pattern).

### 2. The model picker

Mechanics = OpenCode's three-tier fuzzy list + Codex's highlighted-row warning
and hidden-model filtering + Codypendent's classification badge and measured
stats.

```
┌ Select model ─────────────────────────────── data: Internal ─┐
│ > son                                        (fuzzy: model+provider)
│ FAVORITES
│   ● anthropic/claude-sonnet-4     $3/$15   200k  hosted        ● = current
│ RECENT
│   ○ local/qwen2.5-coder-32b       free     32k   local ✓
│ ANTHROPIC
│ > ○ claude-sonnet-4.5            $3/$15   200k  hosted ⚠       ← highlighted
│       ⚠ hosted — routes Internal data off-device (allowed)     selected_description
│   ○ claude-opus-4.1             $15/$75   200k  hosted
│ LOCAL · lm studio
│   ○ gemma-3-27b                   free    128k   local ✓ · 98% · 1.2s   ← measured
│   ⊘ deepseek-r1                   hidden: deprecated
│ ── Connect a provider…
└ ↑↓ move · enter select · / filter · f favorite · esc ─────────┘
```

The header carries the run's `DataClassification` (`data: Internal`). When that
classification forbids a model's location, the row renders **disabled with a
reason** — the same treatment Codex gives legacy models:

```
   ⊘ claude-sonnet-4.5    hosted — blocked for Secret data
```

Selecting a hosted model for classified-but-permitted data triggers Codex's
**two-step gate**: a confirm popup ("routes Internal data off-device —
continue?") so the off-device decision is deliberate, mirroring the routing
hard-filter's fail-closed posture. Selection then chains into a
reasoning/variant sub-popup where applicable, and applies **in-session** and
**persists to config** as two separate `AppEvent`s (Codex), which fits our
approval-gated-writes ethos.

### 3. Provider connect & authentication

A `SignInState` state-machine (Codex) driven by **per-provider, data-defined auth
methods** (OpenCode):

- **API key** (with `…_API_KEY` env-var prefill),
- **OAuth in browser**,
- **device-code** for the headless/daemon case (Codypendent is daemon-centric —
  this matters more for us than for either reference),
- **"Other"** for any OpenAI-compatible/local endpoint.

A `forced_method` supports locked-down installs; the OAuth URL renders as a
sanitised OSC-8 hyperlink (reuse the `codypendent-sandbox` `sanitize` module — the
same control-stripping we apply to untrusted tool output). On success, jump to
the provider-scoped model picker.

**This closes a real gap.** Today the runtime provider layer accepts only
`"openai-compatible"` providers (`crates/runtime/src/models.rs`). Modelling auth
as `{ type: api | oauth, prompts }` data makes subscription/OAuth (Claude
Pro/Max, GitHub Copilot, ChatGPT) *another method variant* rather than a rewrite.

### 4. The differentiator: classification-aware selection

Neither Codex nor OpenCode annotates models by data sensitivity — they show
price/context/capabilities. Codypendent already carries `DataClassification`,
`ModelLocation`, and the routing hard-filter, so the picker can show a
**local/hosted eligibility badge inline** and filter/gate exactly as the engine
would route. The picker becomes a faithful, legible window onto the same
fail-closed decision the router makes headlessly — one column no competitor has.

## Implementation notes

- **Substrate:** port Codex's generic `SelectionItem` / selection-view widget
  into the `codypendent-tui` crate (the command palette can share it). Model,
  reasoning, provider, and the off-device-confirm popups are all instances.
- **Composition with routing/telemetry:** this picker is the client surface over
  the *same* `DataClassification` + `RoutingPolicy` + `ModelProfile` data the
  daemon routing seam (`crates/codypendentd/src/routing.rs`) already consumes.
  The in-flight telemetry work — measured usage → cost budgets, and routing wired
  into workflow-node/eval model selection — is what makes the picker's cost and
  eligibility columns real rather than cosmetic.
- **Per-AgentMode selection:** Codex's Plan-mode reasoning-scope prompt maps onto
  our `AgentMode` (Build/Plan) and workflow-node `model_policy` — a selection can
  apply to the active mode only or globally.
- **Testing:** snapshot-test each popup (Codex uses `insta`); the classification
  gating and the off-device confirm are the correctness-critical cases to pin.

## Non-goals / open questions

- Not a live `models.dev`-style network catalogue in v1; the catalog can start
  from `models.toml` + `model_profiles` and gain a richer metadata source later.
- Whether the off-device confirm should be a hard block (policy) or a
  per-selection prompt is a policy-engine decision, not a TUI one.
- OAuth token storage must go through the existing credential/secret handling,
  not a new store.

## References

- Codex TUI — model/reasoning popups:
  `openai/codex` `codex-rs/tui/src/chatwidget/model_popups.rs`; auth state-machine:
  `codex-rs/tui/src/onboarding/auth.rs`.
- OpenCode TUI — model dialog:
  `anomalyco/opencode` `packages/tui/src/component/dialog-model.tsx`; provider/auth:
  `packages/tui/src/component/dialog-provider.tsx`.
- Broader survey context: `docs/docs/19-competitive-design-synthesis.md`.
