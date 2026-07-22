//! The Phase-7 routing seam (STEP 7.2/7.3 daemon wiring): the one place the
//! daemon asks `codypendent-routing`'s [`Router`] which model to run a task node
//! on, **behind config, default OFF**.
//!
//! `codypendent-routing` is a complete, tested engine with no daemon consumer:
//! it selects a model from a set of measured [`ModelProfile`]s under a versioned
//! [`RoutingPolicy`], applying **security/privacy hard filters before any
//! utility scoring** (classified data can never be scored against — let alone
//! routed to — a hosted provider), then escalates on an objective failure along
//! a declared chain, preserving artifacts. This module is the seam that feeds it
//! real inputs and records its decisions.
//!
//! ## Default OFF
//!
//! Routing is selected by a `<data_dir>/routing.toml` registry item. When that
//! file is absent, unreadable, or `enabled = false`, [`RoutingCoordinator::select`]
//! returns `Ok(None)` and the caller resolves a model exactly as before (the
//! Phase-1 [`resolve_model`](codypendent_runtime::models::resolve_model) path) —
//! so the single-agent baseline and every existing test are unchanged unless
//! routing is explicitly enabled. A malformed `routing.toml` leaves routing OFF
//! (an optimization must never break runs on a typo) with a warning.
//!
//! ## Fail closed
//!
//! When routing IS enabled the seam never silently falls back to the
//! classification-blind Phase-1 resolver: the router refusing to route
//! (`NoEligibleModel` — e.g. classified data with no eligible local model) is
//! surfaced as [`RoutingSeamError::Refused`] so the run fails cleanly rather than
//! leaking off-device. Enabled-but-no-profiles is likewise an error, not a
//! bypass: the operator must `codypendent models bench` a model first.
//!
//! ## The classification path (non-negotiable) — fail closed
//!
//! Every [`TaskNode`] is stamped with a [`DataClassification`]
//! ([`RoutingCoordinator::build_task_node`]) so the engine's `is_eligible` hard
//! filter refuses off-device routing before scoring. That classification is the
//! per-run value when a caller can derive one, falling back to the
//! operator-declared per-scope ceiling in `routing.toml` — which itself
//! **defaults fail-closed to [`DataClassification::Unknown`]** (the most
//! restrictive rank). So enabling routing without declaring a classification
//! keeps work **local-only**, never silently off-device (an under-classification
//! would let classified data reach a hosted model even though the filter itself
//! is correct). `RunLaunch` carries no per-run classification today, so the
//! single-agent executor passes `None` and the config ceiling governs; deriving a
//! real per-run classification (e.g. from a run's declared scope/inputs) is a
//! documented follow-up, gated behind that same fail-closed default. Pinned by
//! `secret_data_never_selects_a_hosted_model_through_the_seam` and
//! `undeclared_classification_defaults_fail_closed_to_local`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use codypendent_daemon::ledger;
use codypendent_daemon::model_profiles::{ModelProfileStore, ModelProfileStoreError};
use codypendent_daemon::subscriptions::SubscriptionHub;
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    Actor, AgentMode, DataClassification, EventBody, ModelId, RunId, SessionId,
};
use codypendent_routing::{
    classify, ModelCapabilities, ModelLocation, ModelProfile, RequiredCapabilities, Router,
    RoutingDecision, RoutingError, RoutingPolicy, RoutingTransition, TaskNode, TaskSignals,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{info, warn};

/// The routing-seam registry item, loaded from `<data_dir>/routing.toml`.
///
/// Its [`RoutingPolicy`] is the versioned `router/<name>/<version>` the decision
/// is attributed to; `data_classification` is the run/scope's declared
/// sensitivity threaded into every [`TaskNode`] (the security hard-filter input).
#[derive(Debug, Clone)]
pub struct RoutingConfig {
    /// Whether the routing seam is active. Default `false` (OFF).
    pub enabled: bool,
    /// The versioned policy the router optimizes under (λ weights, quality
    /// threshold, escalation chain, off-device ceiling).
    pub policy: RoutingPolicy,
    /// The **operator-declared per-scope classification ceiling** — the most
    /// sensitive data runs under this scope are asserted to handle. It is the
    /// FALLBACK threaded into the `TaskNode` when a call supplies no per-run
    /// classification (see [`RoutingCoordinator::select`]).
    ///
    /// **Fail-closed default:** [`DataClassification::Unknown`] — the most
    /// restrictive rank (`rank() == 4`, above `Secret`). An operator who enables
    /// routing without declaring a ceiling therefore gets **local-only** routing
    /// (hosted models are filtered for undeclared data), never silent off-device
    /// routing. To permit hosted models the operator must explicitly declare a
    /// lower ceiling (e.g. `Internal`) in `routing.toml`, an affirmative act.
    pub data_classification: DataClassification,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            policy: RoutingPolicy::balanced(),
            // Fail closed: undeclared data is treated as most-restrictive, so
            // enabling routing without a classification keeps work local.
            data_classification: DataClassification::Unknown,
        }
    }
}

/// The on-disk shape of `routing.toml`. Deserializing `policy` runs
/// [`RoutingPolicy::validate`] (via its `try_from` wire hook), so a malformed
/// policy (NaN/negative λ, out-of-range threshold, duplicate chain id) is
/// rejected here rather than reaching the router.
#[derive(Debug, Deserialize)]
struct RoutingConfigFile {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    policy: Option<RoutingPolicy>,
    #[serde(default)]
    data_classification: Option<DataClassification>,
}

impl RoutingConfig {
    /// Load the routing config from `<data_dir>/routing.toml`. Absent → default
    /// (OFF). A read/parse error is warned and treated as OFF: an optimization
    /// seam must never break runs on a broken config file.
    #[must_use]
    pub fn load(paths: &RuntimePaths) -> Self {
        let path = paths.data_dir.join("routing.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "could not read routing.toml; routing stays OFF");
                return Self::default();
            }
        };
        match toml::from_str::<RoutingConfigFile>(&text) {
            Ok(file) => Self {
                enabled: file.enabled,
                policy: file.policy.unwrap_or_else(RoutingPolicy::balanced),
                // Fail closed when the operator declared no ceiling.
                data_classification: file
                    .data_classification
                    .unwrap_or(DataClassification::Unknown),
            },
            Err(e) => {
                warn!(path = %path.display(), error = %e, "invalid routing.toml; routing stays OFF");
                Self::default()
            }
        }
    }
}

/// A first-use capability probe (STEP 7.2.3): ask a live endpoint what it
/// actually supports (streaming? tools? parallel tools? structured output?).
/// The coordinator caches the result per model+endpoint and treats it as
/// authoritative over the declared capabilities — a model that cannot really
/// call tools is ineligible for a tool-requiring node regardless of its
/// declaration.
#[async_trait]
pub trait CapabilityProber: Send + Sync {
    /// Probe `model` at `endpoint`, or a human reason the probe failed.
    async fn probe(&self, endpoint: &str, model: &ModelId) -> Result<ModelCapabilities, String>;
}

/// A routing failure at the daemon seam.
#[derive(Debug, thiserror::Error)]
pub enum RoutingSeamError {
    /// The router refused to route this node — the *correct* fail-closed outcome
    /// when classified data has no eligible (local) model. Never fall back to the
    /// classification-blind resolver on this.
    #[error("routing refused: {0}")]
    Refused(String),
    /// Routing is enabled but no model profiles are stored — a misconfiguration.
    /// Failing here (rather than falling back) keeps the seam fail-closed: the
    /// Phase-1 resolver is classification-blind, so a silent fallback would
    /// bypass the security hard filter the operator enabled routing for.
    #[error(
        "routing is enabled but no model profiles exist; run `codypendent models bench <id>` first"
    )]
    NoProfiles,
    #[error(transparent)]
    Store(#[from] ModelProfileStoreError),
    #[error(transparent)]
    Ledger(#[from] anyhow::Error),
}

/// The outcome of a successful routing decision: the model chosen, the full
/// [`RoutingDecision`] (recorded in the trace), the [`TaskNode`] retained so a
/// later [`RoutingCoordinator::escalate`] re-routes the SAME node, and the
/// selected model's price (so the node-execution path can price MEASURED tokens
/// into a cost — Phase 7 cost enforcement).
#[derive(Debug, Clone)]
pub struct RoutingSelection {
    pub decision: RoutingDecision,
    /// The task node the decision was made for, retained so a later
    /// [`RoutingCoordinator::escalate`] re-routes the SAME node. Consumed by the
    /// escalation seam, which is tested at the daemon level but not yet driven by
    /// the single-agent live loop (see [`RoutingCoordinator::escalate`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub node: TaskNode,
    /// The selected model's MEASURED blended price per 1K tokens (USD), or `None`
    /// when its price is UNMEASURED. The node-execution path multiplies a `Some`
    /// price by the run's MEASURED total tokens to get the honest `cost_micros`
    /// that `maximum_cost_usd` enforces against; a `None` keeps the node's cost
    /// UNMEASURED (never charged) rather than fabricating a measured `0`.
    ///
    /// The measured-vs-unmeasured rule is `measured_price` (the single source of
    /// truth): a benched HOSTED model's stored `0.0` is the bench's "could not
    /// price this endpoint" sentinel ⇒ `None`, while a LOCAL model's `0.0` is a
    /// genuine free price ⇒ `Some(0.0)`, and any real `> 0` rate ⇒ `Some(rate)`.
    /// This applies the T1/T7 honesty invariant to price: an unmeasured price must
    /// yield an unmeasured cost, never a fabricated free `0` that would silently
    /// defeat the cost budget for the paid hosted models it matters most for.
    pub price_per_1k_usd: Option<f64>,
}

impl RoutingSelection {
    /// The selected model id.
    #[must_use]
    pub fn model(&self) -> &ModelId {
        &self.decision.model
    }
}

/// Interpret a benched model's stored `cost_per_1k_tokens_usd` as a MEASURED
/// price (`Some`) or an UNMEASURED one (`None`), applying the T1/T7 honesty
/// invariant to price. **This is the single source of truth for the rule.**
///
/// `models bench` writes `cost_per_1k_tokens_usd: 0.0` for EVERY model — it is a
/// LOCAL-bench harness and does not (cannot) measure a hosted endpoint's token
/// price (see [`BenchOutcome::into_profile`](codypendent_runtime::bench::BenchOutcome::into_profile);
/// the CLI warns when it benches a non-local endpoint). So a stored `0.0` means
/// two very different things, disambiguated ONLY by where the model runs:
///
/// - **`Hosted` + `0.0` ⇒ `None`** (UNMEASURED): the bench could not price this
///   paid endpoint, so we must NOT fabricate a measured `Some(0)` cost the budget
///   would silently treat as free — the bug this guards against
///   (`maximum_cost_usd` never firing for a benched hosted model). Unmeasured
///   price ⇒ unmeasured cost.
/// - **`Local` + `0.0` ⇒ `Some(0.0)`** (MEASURED): a local model is genuinely
///   free — a real measured zero, distinct from an unmeasured `None`.
/// - **any non-zero price ⇒ `Some(price)`** (MEASURED): a real rate is honest
///   regardless of location (a metered local model, or a hosted profile
///   hand-configured with a real `> 0` price — operator-configured hosted prices
///   are future work, but such a price is already respected here).
#[must_use]
fn measured_price(location: ModelLocation, cost_per_1k_tokens_usd: f64) -> Option<f64> {
    if matches!(location, ModelLocation::Hosted) && cost_per_1k_tokens_usd == 0.0 {
        // Hosted + the bench's `0.0` sentinel: the price is UNMEASURED.
        None
    } else {
        Some(cost_per_1k_tokens_usd)
    }
}

/// The routing seam: routes a task node to a model over the durable profile
/// store, records the decision/escalation into the run trace, and escalates on
/// objective failure. Cheap to clone (pool handle + shared config + optional
/// prober/subscriptions).
#[derive(Clone)]
pub struct RoutingCoordinator {
    pool: SqlitePool,
    config: Arc<RoutingConfig>,
    /// The first-use capability prober, if configured. `None` uses profiles'
    /// declared capabilities as-is (no live probing).
    prober: Option<Arc<dyn CapabilityProber>>,
    /// The fan-out a recorded routing note is published to (so an attached client
    /// sees the decision live), if the seam is bound to one.
    subscriptions: Option<SubscriptionHub>,
}

impl RoutingCoordinator {
    /// Build a coordinator over `pool` with `config`.
    #[must_use]
    pub fn new(pool: SqlitePool, config: RoutingConfig) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            prober: None,
            subscriptions: None,
        }
    }

    /// Attach a first-use capability prober (STEP 7.2.3). Exercised by the
    /// coordinator's own tests; the live single-agent executor does not yet
    /// inject a production prober (declared capabilities are used until it does).
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn with_prober(mut self, prober: Arc<dyn CapabilityProber>) -> Self {
        self.prober = Some(prober);
        self
    }

    /// Publish recorded routing notes to `subscriptions` so an attached client
    /// observes the decision live (persist-then-publish, like the executor's
    /// `emit_note`).
    #[must_use]
    pub fn with_subscriptions(mut self, subscriptions: SubscriptionHub) -> Self {
        self.subscriptions = Some(subscriptions);
        self
    }

    /// Whether the routing seam is active.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Select a model for a task node, or `Ok(None)` when routing is OFF (the
    /// caller then resolves a model the Phase-1 way — unchanged baseline).
    ///
    /// `run_classification` is the run's real [`DataClassification`] when the
    /// caller can derive one, threaded into the `TaskNode` so the engine
    /// hard-filters hosted providers for classified data before any scoring. When
    /// it is `None` (no per-run signal available), the coordinator falls back to
    /// the operator-declared [`RoutingConfig::data_classification`] ceiling, which
    /// itself **defaults fail-closed** to [`DataClassification::Unknown`] — so an
    /// undeclared run never becomes hosted-eligible by default. The per-run value,
    /// when present, always wins over the config ceiling (a caller cannot
    /// accidentally *lower* sensitivity by omitting it).
    pub async fn select(
        &self,
        mode: AgentMode,
        node_kind: &str,
        objective: &str,
        estimated_input_tokens: u64,
        run_classification: Option<DataClassification>,
    ) -> Result<Option<RoutingSelection>, RoutingSeamError> {
        if !self.config.enabled {
            return Ok(None);
        }
        let profiles = self.eligible_profiles().await?;
        if profiles.is_empty() {
            return Err(RoutingSeamError::NoProfiles);
        }
        let node = self.build_task_node(
            mode,
            node_kind,
            objective,
            estimated_input_tokens,
            run_classification,
        );
        let router = Router::new(&profiles, &self.config.policy);
        match router.route(&node) {
            Ok(decision) => {
                info!(
                    model = %decision.model, task_class = ?decision.task_class,
                    policy = %decision.policy_key, classifier = %decision.classifier_version,
                    "routing selected a model"
                );
                // The selected model's price, interpreted through the honesty
                // invariant by `measured_price`: a benched HOSTED model's `0.0` is
                // the bench's "unmeasured" sentinel (⇒ `None`, so the node path
                // never fabricates a measured `Some(0)` cost the budget would treat
                // as free), while a LOCAL model's `0.0` is genuinely free (⇒
                // `Some(0.0)`) and any real `> 0` rate is measured. The router
                // picked `decision.model` FROM `profiles`, so the profile is
                // present; a missing one (the impossible "selected a model not in
                // the pool") is defensively treated as UNMEASURED (`None`).
                let price_per_1k_usd = profiles
                    .iter()
                    .find(|p| p.id == decision.model)
                    .and_then(|p| measured_price(p.location, p.performance.cost_per_1k_tokens_usd));
                Ok(Some(RoutingSelection {
                    decision,
                    node,
                    price_per_1k_usd,
                }))
            }
            Err(RoutingError::NoEligibleModel { reason }) => Err(RoutingSeamError::Refused(reason)),
            Err(other) => Err(RoutingSeamError::Refused(other.to_string())),
        }
    }

    /// Validate an operator-**pinned** model (STEP MP2) against the
    /// classification hard filter — the non-negotiable security boundary a pin
    /// must never bypass.
    ///
    /// - **Routing OFF** (the default): `Ok(())`. No routing security filter is
    ///   active, so the pin selects the model under the existing
    ///   classification-blind Phase-1 posture — no worse than today.
    /// - **Routing ON:** the pin must clear the SAME classification /
    ///   off-device ceiling the router itself applies (built from the run's
    ///   classification, or the operator-declared fail-closed config ceiling).
    ///   An ineligible pin — a hosted model for classified data, or a model with
    ///   no benchmarked profile proving it runs on-device — is **refused**
    ///   ([`RoutingSeamError::Refused`]), exactly like a routing refusal, so the
    ///   run fails CLOSED rather than leaking off-device. A pin overrides the
    ///   router's *quality* judgment, never this *security* constraint.
    pub async fn validate_pin(
        &self,
        mode: AgentMode,
        node_kind: &str,
        objective: &str,
        estimated_input_tokens: u64,
        run_classification: Option<DataClassification>,
        pinned: &ModelId,
    ) -> Result<(), RoutingSeamError> {
        if !self.config.enabled {
            return Ok(());
        }
        let profiles = self.eligible_profiles().await?;
        let node = self.build_task_node(
            mode,
            node_kind,
            objective,
            estimated_input_tokens,
            run_classification,
        );
        let router = Router::new(&profiles, &self.config.policy);
        if router.model_passes_classification(pinned, &node) {
            Ok(())
        } else {
            Err(RoutingSeamError::Refused(format!(
                "pinned model {pinned} may not process this run's data \
                 (classification {:?}): it is a hosted/off-device model above the \
                 policy's ceiling, or it has no benchmarked profile proving it runs \
                 on-device — run `codypendent models bench {pinned}` or pin a local model",
                node.data_classification
            )))
        }
    }

    /// Escalate after an objective validation failure: advance the policy's
    /// escalation chain to the next eligible tier past `from`, re-routing the
    /// SAME node. The returned [`RoutingTransition`] is stamped
    /// `artifacts_preserved: Some(true)` — the daemon re-executes the same run id
    /// against the new model and the run's artifacts/blackboard are durable in
    /// the store, so they genuinely survive the switch (the honest execution-time
    /// fact the engine itself cannot assert, P7-4).
    ///
    /// **Wired and tested at this seam; the LIVE re-drive awaits the runtime's
    /// mid-run model-switch hook.** Re-driving [`FrameworkAgentRuntime::execute_run`]
    /// today would emit a *second* terminal `RunCompleted`, breaking the
    /// "`RunCompleted` is terminal" contract clients stream against — so this
    /// (and [`Self::record_transition`]) is exercised through the daemon seam's
    /// tests, not the single-agent live loop, until the runtime grows the
    /// "model-execution seam" the ROADMAP flags as remaining for STEP 7.3.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn escalate(
        &self,
        from: &ModelId,
        reason: impl Into<String>,
        node: &TaskNode,
    ) -> Result<(RoutingDecision, RoutingTransition), RoutingSeamError> {
        let profiles = self.eligible_profiles().await?;
        let router = Router::new(&profiles, &self.config.policy);
        match router.escalate(from, reason, node) {
            Ok((decision, mut transition)) => {
                // The daemon re-executes the same run preserving durable artifacts,
                // so it can honestly assert what the engine (which executes nothing)
                // could not.
                transition.artifacts_preserved = Some(true);
                Ok((decision, transition))
            }
            Err(RoutingError::EscalationExhausted { from }) => Err(RoutingSeamError::Refused(
                format!("escalation chain exhausted after {from}"),
            )),
            Err(other) => Err(RoutingSeamError::Refused(other.to_string())),
        }
    }

    /// Record a routing decision into `run`'s trace as a durable `NoteAppended`
    /// (persist-then-publish, mirroring the executor's context/memory notes), so
    /// the selection — model, task class, classifier version, policy revision,
    /// and the numbers — is attributable from the run's own event history.
    pub async fn record_decision(
        &self,
        session: SessionId,
        run: RunId,
        decision: &RoutingDecision,
    ) -> Result<(), RoutingSeamError> {
        self.emit_note(session, run, render_decision(decision))
            .await
    }

    /// Record an escalation transition into `run`'s trace (old/new model, reason,
    /// context transformation, cost impact, artifacts-preserved). Pairs with
    /// [`Self::escalate`]; see its note on the live re-drive seam.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn record_transition(
        &self,
        session: SessionId,
        run: RunId,
        transition: &RoutingTransition,
    ) -> Result<(), RoutingSeamError> {
        self.emit_note(session, run, render_transition(transition))
            .await
    }

    /// Build a [`TaskNode`] from a run's task signals — the classification path.
    /// The rule-based classifier (version-stamped, recorded in the decision) maps
    /// mode + node kind + input size + objective keywords to a task class; the
    /// required capabilities are derived from the mode; and the node's
    /// `data_classification` is the per-run value when supplied, else the
    /// operator-declared config ceiling (fail-closed [`DataClassification::Unknown`]
    /// by default) — so the security hard filter can refuse off-device routing.
    fn build_task_node(
        &self,
        mode: AgentMode,
        node_kind: &str,
        objective: &str,
        estimated_input_tokens: u64,
        run_classification: Option<DataClassification>,
    ) -> TaskNode {
        let classification = classify(&TaskSignals::from_objective(
            mode_str(mode),
            node_kind,
            estimated_input_tokens,
            objective,
        ));
        TaskNode {
            classification,
            required: required_capabilities(mode),
            // Per-run wins over the config ceiling; the config ceiling itself
            // defaults fail-closed, so an undeclared run is never under-classified.
            data_classification: run_classification.unwrap_or(self.config.data_classification),
            estimated_input_tokens,
            estimated_output_tokens: estimated_output_tokens(mode),
        }
    }

    /// The eligible model pool: every stored profile, with its capabilities
    /// replaced by the first-use probe result when one is available (the probe is
    /// authoritative over the declared capabilities — STEP 7.2.3). Probes a model
    /// that has never been probed when a prober is configured, caching the result.
    async fn eligible_profiles(&self) -> Result<Vec<ModelProfile>, RoutingSeamError> {
        let store = ModelProfileStore::new();
        let stored = store.list(&self.pool).await?;
        let mut profiles = Vec::with_capacity(stored.len());
        for entry in stored {
            let mut profile = entry.profile;
            let probed = self
                .probed_capabilities(&store, &profile.id, &entry.endpoint)
                .await;
            if let Some(caps) = probed {
                // The probe is authoritative: a denied feature makes the model
                // ineligible for a node that requires it.
                profile.capabilities = caps;
            }
            profiles.push(profile);
        }
        Ok(profiles)
    }

    /// The probed capabilities for `(model, endpoint)`: the cached value if
    /// present, otherwise — when a prober is configured — a fresh probe that is
    /// cached for next time. `None` means "no probe available" (no prober, or the
    /// probe failed), so the caller keeps the declared capabilities.
    async fn probed_capabilities(
        &self,
        store: &ModelProfileStore,
        model: &ModelId,
        endpoint: &str,
    ) -> Option<ModelCapabilities> {
        match store.cached_capabilities(&self.pool, model, endpoint).await {
            Ok(Some(caps)) => return Some(caps),
            Ok(None) => {}
            Err(error) => {
                warn!(%model, endpoint, %error, "could not read cached capabilities; using declared");
                return None;
            }
        }
        let prober = self.prober.as_ref()?;
        match prober.probe(endpoint, model).await {
            Ok(caps) => {
                if let Err(error) = store
                    .cache_capabilities(&self.pool, model, endpoint, &caps)
                    .await
                {
                    warn!(%model, endpoint, %error, "could not cache the capability probe");
                }
                Some(caps)
            }
            Err(reason) => {
                warn!(%model, endpoint, %reason, "capability probe failed; using declared capabilities");
                None
            }
        }
    }

    /// Append a `NoteAppended` (carrying `run`) to `session`'s ledger and, if a
    /// fan-out is bound, publish it — persist-then-publish, exactly as the
    /// executor's `emit_note`.
    async fn emit_note(
        &self,
        session: SessionId,
        run: RunId,
        text: String,
    ) -> Result<(), RoutingSeamError> {
        let event = ledger::append_next_event(
            &self.pool,
            session,
            &Actor::System,
            &EventBody::NoteAppended {
                text,
                run_id: Some(run),
            },
            Utc::now(),
        )
        .await?;
        if let Some(subscriptions) = &self.subscriptions {
            subscriptions.publish(session, event);
        }
        Ok(())
    }
}

/// A rough input-token estimate for an objective (a heuristic, not a tokenizer):
/// ~4 bytes/token, floored at a small minimum so a terse objective still routes
/// through the size filter. The real run context (repo map, cards, memories) is
/// larger, so this under-counts — a conservative floor the router can only
/// tighten, never a fabricated precise figure.
#[must_use]
pub fn estimate_input_tokens(objective: &str) -> u64 {
    ((objective.len() as u64) / 4).max(256)
}

/// The output-size estimate the size hard filter reserves room for, by mode: a
/// writing mode (`Build`) generates more (patches) than a read-only mode.
fn estimated_output_tokens(mode: AgentMode) -> u64 {
    match mode {
        AgentMode::Build => 4_000,
        _ => 1_000,
    }
}

/// What a run in `mode` requires of a model. Every mode drives tools; a writing
/// mode additionally needs reliable structured output (structured patch/edit
/// calls). The size minimums stay at their defaults — the router folds the
/// node's own token estimates into the fit check.
fn required_capabilities(mode: AgentMode) -> RequiredCapabilities {
    RequiredCapabilities {
        tools: true,
        structured_output: matches!(mode, AgentMode::Build),
        ..Default::default()
    }
}

/// The lowercase mode string the rule-based classifier reads. An unknown
/// (forward-compat) mode maps to the empty string, which the classifier treats
/// as "no mode signal".
fn mode_str(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Build => "build",
        AgentMode::Explore => "explore",
        AgentMode::Ask => "ask",
        AgentMode::Plan => "plan",
        AgentMode::Review => "review",
        _ => "",
    }
}

/// Render a [`RoutingDecision`] as a legible, attributable trace note.
fn render_decision(d: &RoutingDecision) -> String {
    format!(
        "routing: selected `{}` for task-class `{}` via `{}` (classifier `{}`, {:?}); \
         predicted_success={:.3}, expected_cost_usd={:.5}, expected_latency_ms={:.0}, utility={:.4}",
        d.model.0,
        d.task_class.as_str(),
        d.policy_key,
        d.classifier_version,
        d.reason,
        d.predicted_success,
        d.expected_cost_usd,
        d.expected_latency_ms,
        d.utility,
    )
}

/// Render a [`RoutingTransition`] as a legible, attributable trace note.
fn render_transition(t: &RoutingTransition) -> String {
    format!(
        "routing escalation: `{}` -> `{}` ({}); context_transformation=\"{}\", \
         cost_impact_usd={:.5}, artifacts_preserved={}",
        t.from.0,
        t.to.0,
        t.reason,
        t.context_transformation,
        t.cost_impact_usd,
        t.artifacts_preserved
            .map_or("unreported".to_string(), |p| p.to_string()),
    )
}

/// Seed a session + run row so a routing note's `run_id` resolves and
/// `append_next_event` has a session to append to.
#[cfg(test)]
async fn seed_session_run(pool: &SqlitePool) -> (SessionId, RunId) {
    use codypendent_daemon::projections;
    let session = SessionId::new();
    let run = RunId::new();
    ledger::create_session(pool, session, "routing-seam-test")
        .await
        .unwrap();
    projections::insert_run(
        pool,
        run,
        session,
        "diagnose",
        AgentMode::Build,
        "router",
        "{}",
    )
    .await
    .unwrap();
    (session, run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_routing::{
        Lambdas, ModelCapabilities, ModelExecutionProfile, ModelLocation, ModelPerformance,
        StructuredOutputSupport, ToolCallSupport,
    };
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempdir().unwrap();
        let pool = codypendent_daemon::db::open_database(&dir.path().join("test.db"))
            .await
            .unwrap();
        (dir, pool)
    }

    fn caps(context: u64, tools: ToolCallSupport) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tools,
            parallel_tools: matches!(tools, ToolCallSupport::Parallel),
            structured_output: StructuredOutputSupport::Strict,
            vision: false,
            audio_input: false,
            embeddings: false,
            prompt_caching: false,
            reasoning_controls: false,
            context_tokens: Some(context),
            output_tokens: Some(16_000),
        }
    }

    fn profile(id: &str, location: ModelLocation, reliability: f64, cost: f64) -> ModelProfile {
        ModelProfile {
            id: ModelId(id.into()),
            location,
            capabilities: caps(200_000, ToolCallSupport::Parallel),
            performance: ModelPerformance {
                reliability,
                cost_per_1k_tokens_usd: cost,
                latency_ms_p50: 700.0,
                task_class_success: BTreeMap::new(),
                failure_patterns: vec![],
            },
            execution: ModelExecutionProfile::default(),
            bench: None,
        }
    }

    fn policy_with(chain: Vec<&str>, max_off_device: DataClassification) -> RoutingPolicy {
        let mut policy = RoutingPolicy::balanced();
        policy.name = "coding".into();
        policy.lambdas = Lambdas::default();
        policy.escalation_chain = chain.into_iter().map(|s| ModelId(s.into())).collect();
        policy.max_off_device = max_off_device;
        policy
    }

    async fn store_profiles(pool: &SqlitePool, profiles: &[(&str, ModelProfile)]) {
        let store = ModelProfileStore::new();
        for (endpoint, profile) in profiles {
            store.upsert(pool, endpoint, profile).await.unwrap();
        }
    }

    #[tokio::test]
    async fn disabled_by_default_returns_none() {
        let (_dir, pool) = pool().await;
        let coord = RoutingCoordinator::new(pool, RoutingConfig::default());
        assert!(!coord.enabled());
        let selection = coord
            .select(AgentMode::Build, "agent", "fix the bug", 4_000, None)
            .await
            .unwrap();
        assert!(
            selection.is_none(),
            "OFF by default: caller falls back to resolve_model"
        );
    }

    #[tokio::test]
    async fn enabled_but_no_profiles_fails_closed() {
        let (_dir, pool) = pool().await;
        let config = RoutingConfig {
            enabled: true,
            ..RoutingConfig::default()
        };
        let coord = RoutingCoordinator::new(pool, config);
        let err = coord
            .select(AgentMode::Build, "agent", "fix the bug", 4_000, None)
            .await
            .unwrap_err();
        assert!(matches!(err, RoutingSeamError::NoProfiles));
    }

    #[tokio::test]
    async fn routes_to_the_cheapest_model_above_threshold_and_records_it() {
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[
                (
                    "https://a/v1",
                    profile("cheap", ModelLocation::Hosted, 0.80, 0.002),
                ),
                (
                    "https://b/v1",
                    profile("mid", ModelLocation::Hosted, 0.88, 0.010),
                ),
                (
                    "https://c/v1",
                    profile("strong", ModelLocation::Hosted, 0.95, 0.030),
                ),
            ],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Confidential),
            data_classification: DataClassification::Internal,
        };
        let coord = RoutingCoordinator::new(pool.clone(), config);
        let (session, run) = seed_session_run(&pool).await;

        let selection = coord
            .select(
                AgentMode::Build,
                "agent",
                "fix the off-by-one bug",
                4_000,
                None,
            )
            .await
            .unwrap()
            .expect("routing ON selects a model");
        assert_eq!(
            selection.model(),
            &ModelId("cheap".into()),
            "cheapest-above-threshold"
        );
        // The selection surfaces the SELECTED model's MEASURED price, so the node
        // path can price measured tokens into an enforced cost (Phase 7). `cheap`
        // is a HOSTED model with a real `> 0` price ($0.002/1k), so it is measured.
        assert_eq!(
            selection.price_per_1k_usd,
            Some(0.002),
            "the selection carries the chosen model's measured price"
        );
        coord
            .record_decision(session, run, &selection.decision)
            .await
            .unwrap();

        // The decision is a durable trace note carrying the model + policy key.
        let events = ledger::load_events(&pool, session).await.unwrap();
        let note = events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::NoteAppended {
                    text,
                    run_id: Some(r),
                } if *r == run => Some(text),
                _ => None,
            })
            .expect("a routing note is recorded in the trace");
        assert!(
            note.contains("selected `cheap`"),
            "note names the model: {note}"
        );
        assert!(
            note.contains("router/coding/1"),
            "note is attributable to the policy: {note}"
        );
    }

    #[test]
    fn measured_price_is_unmeasured_only_for_a_hosted_zero() {
        // The honesty invariant applied to price (the single source of truth): a
        // benched HOSTED model's `0.0` is the bench's "could not price this
        // endpoint" sentinel ⇒ UNMEASURED (`None`, never charged); a LOCAL `0.0` is
        // a genuine measured free price; and any real `> 0` rate is measured
        // regardless of location.
        assert_eq!(
            measured_price(ModelLocation::Hosted, 0.0),
            None,
            "hosted + 0.0 is UNMEASURED — the bench cannot price a hosted endpoint"
        );
        assert_eq!(
            measured_price(ModelLocation::Local, 0.0),
            Some(0.0),
            "local + 0.0 is a genuine measured free price"
        );
        assert_eq!(
            measured_price(ModelLocation::Hosted, 3.0),
            Some(3.0),
            "a real >0 hosted price is measured (e.g. a hand-configured profile)"
        );
        assert_eq!(
            measured_price(ModelLocation::Local, 2.0),
            Some(2.0),
            "a real >0 local price is measured"
        );
    }

    #[tokio::test]
    async fn a_benched_hosted_selection_carries_an_unmeasured_price() {
        // THE fix at the seam: a benched HOSTED model (its stored
        // `cost_per_1k_tokens_usd` is the bench's `0.0` "unmeasured" sentinel —
        // `models bench` does not price a hosted endpoint) is selected, and the
        // selection carries `price_per_1k_usd == None` (UNMEASURED). So the node
        // path never fabricates a measured `Some(0)` cost `maximum_cost_usd` would
        // silently treat as free. Contrast
        // `routes_to_the_cheapest_model_above_threshold_and_records_it`, where the
        // hosted model has a real `> 0` price and is therefore measured.
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[(
                "https://hosted/v1",
                profile("hosted-benched", ModelLocation::Hosted, 0.90, 0.0),
            )],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Confidential),
            data_classification: DataClassification::Internal,
        };
        let coord = RoutingCoordinator::new(pool, config);

        let selection = coord
            .select(AgentMode::Build, "agent", "do the work", 4_000, None)
            .await
            .unwrap()
            .expect("routing ON selects the benched hosted model");
        assert_eq!(selection.model(), &ModelId("hosted-benched".into()));
        assert_eq!(
            selection.price_per_1k_usd, None,
            "a benched hosted model's 0.0 is the bench's UNMEASURED sentinel, not a \
             fabricated free price"
        );
    }

    #[tokio::test]
    async fn secret_data_never_selects_a_hosted_model_through_the_seam() {
        // THE seam-level leak test: a run whose real classification is Secret,
        // under a policy that only allows Internal off-device, must never select a
        // hosted model. The hosted model here is BOTH strictly more reliable AND
        // strictly CHEAPER than the local one (hosted $0.001/1k < local $0.010/1k),
        // so neither cost nor quality can explain local winning — ONLY the
        // classification hard filter (fed the run's real Secret classification by
        // the daemon) can.
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[
                (
                    "https://hosted/v1",
                    profile("hosted-strong", ModelLocation::Hosted, 0.99, 0.001),
                ),
                (
                    "http://localhost/v1",
                    profile("local", ModelLocation::Local, 0.75, 0.010),
                ),
            ],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Internal),
            data_classification: DataClassification::Secret,
        };
        let coord = RoutingCoordinator::new(pool, config);

        let selection = coord
            .select(
                AgentMode::Build,
                "agent",
                "handle the secret payload",
                4_000,
                None,
            )
            .await
            .unwrap()
            .expect("a local model can serve secret data");
        assert_eq!(
            selection.model(),
            &ModelId("local".into()),
            "Secret data stays on-device: the hosted model is filtered before scoring"
        );
    }

    #[tokio::test]
    async fn secret_data_with_no_local_model_refuses_rather_than_leaks() {
        // No local model + Secret data + restrictive policy ⇒ the seam REFUSES
        // (fail closed), never a fallback that could send the data off-device.
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[(
                "https://hosted/v1",
                profile("hosted", ModelLocation::Hosted, 0.99, 0.001),
            )],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Internal),
            data_classification: DataClassification::Secret,
        };
        let coord = RoutingCoordinator::new(pool, config);
        let err = coord
            .select(
                AgentMode::Build,
                "agent",
                "handle the secret payload",
                4_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RoutingSeamError::Refused(_)),
            "classified data with no eligible model fails closed, got {err:?}"
        );
    }

    // -- Pinned-model classification safety (STEP MP2) ----------------------

    #[tokio::test]
    async fn validate_pin_off_by_default_allows_any_model() {
        // Routing OFF (the default): a pin is honored under the classification-
        // blind Phase-1 posture — no security filter is active, so even a hosted
        // model id validates with no stored profiles at all. No worse than today.
        let (_dir, pool) = pool().await;
        let coord = RoutingCoordinator::new(pool, RoutingConfig::default());
        assert!(!coord.enabled());
        coord
            .validate_pin(
                AgentMode::Build,
                "agent",
                "handle the payload",
                4_000,
                None,
                &ModelId("hosted-anything".into()),
            )
            .await
            .expect("routing OFF accepts any pin");
    }

    #[tokio::test]
    async fn validate_pin_refuses_a_hosted_model_for_classified_data_fail_closed() {
        // THE pinned-model leak test (STEP MP2): with routing ON, a run whose
        // classification exceeds the policy's off-device ceiling must REFUSE a
        // pinned HOSTED model. A pin overrides the router's *quality* judgment but
        // never the classification hard filter — fail closed, exactly like a
        // routing refusal, rather than run the classified data off-device. Same
        // config as `secret_data_never_selects_a_hosted_model_through_the_seam`.
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[
                (
                    "https://hosted/v1",
                    profile("hosted-strong", ModelLocation::Hosted, 0.99, 0.001),
                ),
                (
                    "http://localhost/v1",
                    profile("local", ModelLocation::Local, 0.75, 0.010),
                ),
            ],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Internal),
            data_classification: DataClassification::Secret,
        };
        let coord = RoutingCoordinator::new(pool, config);
        let err = coord
            .validate_pin(
                AgentMode::Build,
                "agent",
                "handle the secret payload",
                4_000,
                None,
                &ModelId("hosted-strong".into()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RoutingSeamError::Refused(_)),
            "a hosted pin for Secret data fails closed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn validate_pin_allows_a_local_model_for_classified_data() {
        // The counterpart: a LOCAL pinned model serves the same Secret data — it
        // never leaves the device, so the classification filter admits it.
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[(
                "http://localhost/v1",
                profile("local", ModelLocation::Local, 0.75, 0.010),
            )],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Internal),
            data_classification: DataClassification::Secret,
        };
        let coord = RoutingCoordinator::new(pool, config);
        coord
            .validate_pin(
                AgentMode::Build,
                "agent",
                "handle the secret payload",
                4_000,
                None,
                &ModelId("local".into()),
            )
            .await
            .expect("a local pin serves classified data on-device");
    }

    #[tokio::test]
    async fn validate_pin_refuses_an_unprofiled_model_when_routing_on() {
        // With routing ON, a pin the profile store has never benchmarked cannot be
        // proven on-device, so it fails closed — the operator must bench it (or pin
        // a known-local model). This keeps an unknown pin from bypassing the
        // security filter under the (opt-in) routing posture.
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[(
                "http://localhost/v1",
                profile("local", ModelLocation::Local, 0.75, 0.010),
            )],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Confidential),
            data_classification: DataClassification::Internal,
        };
        let coord = RoutingCoordinator::new(pool, config);
        let err = coord
            .validate_pin(
                AgentMode::Build,
                "agent",
                "fix the bug",
                4_000,
                None,
                &ModelId("never-benched".into()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RoutingSeamError::Refused(_)),
            "an unprofiled pin fails closed under routing, got {err:?}"
        );
    }

    #[tokio::test]
    async fn validate_pin_allows_a_hosted_model_when_the_policy_permits() {
        // A pin is not blanket-refused under routing: when the run's classification
        // is within the policy's off-device ceiling, a hosted pin is admitted (the
        // pin's whole point is to override the router's *quality* choice, which is
        // fine once the security filter is satisfied).
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[(
                "https://hosted/v1",
                profile("hosted", ModelLocation::Hosted, 0.90, 0.005),
            )],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Confidential),
            data_classification: DataClassification::Internal,
        };
        let coord = RoutingCoordinator::new(pool, config);
        coord
            .validate_pin(
                AgentMode::Build,
                "agent",
                "fix the bug",
                4_000,
                None,
                &ModelId("hosted".into()),
            )
            .await
            .expect("a hosted pin within the off-device ceiling is allowed");
    }

    #[tokio::test]
    async fn escalation_advances_the_chain_and_records_the_transition_with_artifacts_preserved() {
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[
                (
                    "http://localhost/v1",
                    profile("local-default", ModelLocation::Local, 0.75, 0.0),
                ),
                (
                    "https://h1/v1",
                    profile("hosted-default", ModelLocation::Hosted, 0.85, 0.010),
                ),
                (
                    "https://h2/v1",
                    profile("hosted-strong", ModelLocation::Hosted, 0.96, 0.030),
                ),
            ],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(
                vec!["local-default", "hosted-default", "hosted-strong"],
                DataClassification::Confidential,
            ),
            data_classification: DataClassification::Internal,
        };
        let coord = RoutingCoordinator::new(pool.clone(), config);
        let (session, run) = seed_session_run(&pool).await;

        let selection = coord
            .select(
                AgentMode::Build,
                "agent",
                "fix the failing test",
                4_000,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let (decision, transition) = coord
            .escalate(selection.model(), "tests still failing", &selection.node)
            .await
            .unwrap();
        assert_eq!(
            decision.model,
            ModelId("hosted-default".into()),
            "next tier"
        );
        assert_eq!(
            transition.artifacts_preserved,
            Some(true),
            "the daemon preserves the run's durable artifacts across the re-execution"
        );

        coord
            .record_transition(session, run, &transition)
            .await
            .unwrap();
        let events = ledger::load_events(&pool, session).await.unwrap();
        let note = events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::NoteAppended {
                    text,
                    run_id: Some(r),
                } if *r == run => Some(text),
                _ => None,
            })
            .expect("the escalation transition is recorded");
        assert!(
            note.contains("local-default` -> `hosted-default"),
            "note: {note}"
        );
        assert!(note.contains("artifacts_preserved=true"), "note: {note}");
    }

    #[tokio::test]
    async fn a_first_use_probe_that_denies_tools_filters_the_model() {
        // STEP 7.2.3: the first-use capability probe is authoritative. A model
        // that DECLARES parallel tools but whose probe reports no tool support is
        // filtered out of a tool-requiring (Build) node — proving the probe feeds
        // routing, cached per model+endpoint.
        struct DenyToolsProber;
        #[async_trait]
        impl CapabilityProber for DenyToolsProber {
            async fn probe(
                &self,
                _endpoint: &str,
                model: &ModelId,
            ) -> Result<ModelCapabilities, String> {
                // The "toolless" endpoint denies tools; everyone else keeps theirs.
                if model.0 == "toolless" {
                    Ok(caps(200_000, ToolCallSupport::None))
                } else {
                    Ok(caps(200_000, ToolCallSupport::Parallel))
                }
            }
        }

        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[
                // "toolless" is cheaper, so without the probe it would win.
                (
                    "https://x/v1",
                    profile("toolless", ModelLocation::Hosted, 0.90, 0.001),
                ),
                (
                    "https://y/v1",
                    profile("toolful", ModelLocation::Hosted, 0.85, 0.010),
                ),
            ],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Confidential),
            data_classification: DataClassification::Internal,
        };
        let coord =
            RoutingCoordinator::new(pool.clone(), config).with_prober(Arc::new(DenyToolsProber));

        let selection = coord
            .select(AgentMode::Build, "agent", "fix the bug", 4_000, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            selection.model(),
            &ModelId("toolful".into()),
            "the probe denied tools for the cheaper model, filtering it out"
        );
        // The probe was cached per model+endpoint (first-use).
        let cached = ModelProfileStore::new()
            .cached_capabilities(&pool, &ModelId("toolless".into()), "https://x/v1")
            .await
            .unwrap();
        assert_eq!(cached, Some(caps(200_000, ToolCallSupport::None)));
    }

    #[tokio::test]
    async fn undeclared_classification_defaults_fail_closed_to_local() {
        // Fail-closed pin: routing ON, NO classification declared (the config
        // default is `Unknown`, the most-restrictive rank), and NO per-run value
        // (`select(.., None)`). The hosted model is cheaper AND more reliable, but
        // an undeclared run must NOT be treated as low-sensitivity — it stays
        // local. An operator who enables routing without declaring a ceiling never
        // silently gets off-device routing.
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[
                (
                    "https://hosted/v1",
                    profile("hosted-cheap", ModelLocation::Hosted, 0.99, 0.001),
                ),
                (
                    "http://localhost/v1",
                    profile("local", ModelLocation::Local, 0.75, 0.010),
                ),
            ],
        )
        .await;
        // `..RoutingConfig::default()` leaves `data_classification` at the
        // fail-closed `Unknown` default; the balanced policy allows only up to
        // `Confidential` off-device, so `Unknown` (rank 4) is refused off-device.
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Confidential),
            ..RoutingConfig::default()
        };
        assert_eq!(config.data_classification, DataClassification::Unknown);
        let coord = RoutingCoordinator::new(pool, config);

        let selection = coord
            .select(AgentMode::Build, "agent", "do the work", 4_000, None)
            .await
            .unwrap()
            .expect("a local model can serve undeclared data");
        assert_eq!(
            selection.model(),
            &ModelId("local".into()),
            "undeclared data defaults fail-closed: hosted is filtered even though cheaper + better"
        );
    }

    #[tokio::test]
    async fn a_per_run_classification_overrides_the_config_ceiling() {
        // A caller that CAN derive a per-run classification passes it, and it wins
        // over the config ceiling. Here the config ceiling is permissive (Public),
        // but the per-run value is Secret ⇒ the hosted model is still filtered.
        let (_dir, pool) = pool().await;
        store_profiles(
            &pool,
            &[
                (
                    "https://hosted/v1",
                    profile("hosted", ModelLocation::Hosted, 0.99, 0.001),
                ),
                (
                    "http://localhost/v1",
                    profile("local", ModelLocation::Local, 0.75, 0.010),
                ),
            ],
        )
        .await;
        let config = RoutingConfig {
            enabled: true,
            policy: policy_with(vec![], DataClassification::Internal),
            data_classification: DataClassification::Public, // permissive ceiling
        };
        let coord = RoutingCoordinator::new(pool, config);

        let selection = coord
            .select(
                AgentMode::Build,
                "agent",
                "handle secrets",
                4_000,
                Some(DataClassification::Secret), // the real per-run value wins
            )
            .await
            .unwrap()
            .expect("local serves the secret run");
        assert_eq!(
            selection.model(),
            &ModelId("local".into()),
            "the per-run Secret classification overrides the permissive config ceiling"
        );
    }

    #[test]
    fn load_ignores_a_garbage_routing_toml_and_stays_off() {
        // A malformed `routing.toml` must leave routing OFF (warn, never panic) —
        // an optimization seam must not break runs on a broken config file.
        let dir = tempdir().unwrap();
        let paths =
            codypendent_protocol::discovery::RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        std::fs::write(
            paths.data_dir.join("routing.toml"),
            "this is not = valid [ toml ][[",
        )
        .unwrap();
        let config = RoutingConfig::load(&paths);
        assert!(!config.enabled, "a garbage routing.toml leaves routing OFF");
        assert_eq!(
            config.data_classification,
            DataClassification::Unknown,
            "and the fail-closed classification default holds"
        );
    }

    #[test]
    fn load_is_off_when_routing_toml_is_absent() {
        let dir = tempdir().unwrap();
        let paths =
            codypendent_protocol::discovery::RuntimePaths::from_data_dir(dir.path().to_path_buf());
        assert!(!RoutingConfig::load(&paths).enabled);
    }

    #[test]
    fn load_reads_an_enabled_routing_toml() {
        // The happy path parses: an enabled seam with a declared classification.
        let dir = tempdir().unwrap();
        let paths =
            codypendent_protocol::discovery::RuntimePaths::from_data_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        std::fs::write(
            paths.data_dir.join("routing.toml"),
            r#"
enabled = true

[data_classification]
type = "Internal"

[policy]
name = "coding"
version = 3
quality_threshold = 0.7
max_off_device = { type = "Confidential" }

[policy.lambdas]
cost = 1.0
latency = 0.05
privacy = 0.5
failure = 0.5
"#,
        )
        .unwrap();
        let config = RoutingConfig::load(&paths);
        assert!(config.enabled);
        assert_eq!(config.data_classification, DataClassification::Internal);
        assert_eq!(config.policy.registry_key(), "router/coding/3");
    }
}
