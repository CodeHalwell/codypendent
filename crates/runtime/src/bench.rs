//! The local-model benchmark harness (Phase 7 STEP 7.2.2) and first-use
//! capability probes (STEP 7.2.3): `codypendent models bench <id>`.
//!
//! Chapter 09 is emphatic that routing reads **measured** numbers, never vibes.
//! This harness measures a model's Chapter-09 local profile — tokens/sec,
//! time-to-first-token, warm-up, memory, context limit, structured-output
//! reliability, tool-call accuracy (scripted probes), and a small coding-eval
//! score — and produces a [`codypendent_routing::LocalBench`] +
//! [`codypendent_routing::ModelCapabilities`] the daemon persists to the model
//! profile store (migration 0014).
//!
//! ## The [`BenchTarget`] seam
//!
//! The harness is written against a [`BenchTarget`] trait — the model-level test
//! double this repo already uses for model runs is the trait-level
//! [`ScriptedDriver`](crate::agent::ScriptedDriver), not an HTTP server, so the
//! bench follows the same pattern with its own seam. A [`MockBenchTarget`] drives
//! the whole harness **without a real model or network** (the mock-driven test),
//! advertising or denying features per scenario for the capability probe; a
//! production [`DriverBenchTarget`] wraps any [`ModelDriver`](crate::agent::ModelDriver)
//! for real (if seam-limited — see its docs) numbers when an endpoint is
//! configured.

use std::time::Duration;

use async_trait::async_trait;
use codypendent_protocol::ids::ModelId;
use codypendent_routing::{
    LocalBench, ModelCapabilities, ModelExecutionProfile, ModelLocation, ModelPerformance,
    ModelProfile,
};

use crate::agent::{ModelDriver, ModelStep, NullDeltaSink, TurnItem};

/// One timed generation: how many tokens were produced, how long until the first
/// token appeared, and the total wall time.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationSample {
    pub tokens: u64,
    pub time_to_first_token: Duration,
    pub total: Duration,
}

/// What a [`BenchTarget`] reports about itself beyond timing: its advertised
/// capabilities (the first-use capability probe), its context-window limit, and
/// its resident memory footprint (meaningful for a local subprocess model; a
/// hosted/mock target reports what it knows, `0` when it cannot measure it).
#[derive(Debug, Clone, PartialEq)]
pub struct TargetDescription {
    pub capabilities: ModelCapabilities,
    pub context_limit: u64,
    pub memory_mb: u64,
}

/// How many scripted probes each dimension runs. Defaults to the Chapter-09
/// small-eval sizes (10 coding tasks; a handful of structured-output/tool-call
/// probes).
#[derive(Debug, Clone, Copy)]
pub struct BenchOptions {
    pub structured_output_probes: u32,
    pub tool_call_probes: u32,
    pub coding_eval_tasks: u32,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            structured_output_probes: 10,
            tool_call_probes: 10,
            coding_eval_tasks: 10,
        }
    }
}

/// A benchmarkable model. Production wraps a real endpoint; tests use
/// [`MockBenchTarget`]. Every method returns a human reason on failure so the
/// harness surfaces *why* a bench could not complete (an unreachable endpoint,
/// a refused probe) rather than fabricating a number.
#[async_trait]
pub trait BenchTarget: Send + Sync {
    /// The model id this target measures.
    fn model_id(&self) -> ModelId;

    /// The target's advertised capabilities, context limit, and memory footprint.
    async fn describe(&self) -> Result<TargetDescription, String>;

    /// A timed generation. `warm` is `false` for the first (cold) call — the
    /// harness derives warm-up from the cold/warm delta.
    async fn timed_generation(&self, warm: bool) -> Result<GenerationSample, String>;

    /// Run `n` structured-output probes; return how many produced schema-valid
    /// output (`0..=n`).
    async fn structured_output_probe(&self, n: u32) -> Result<u32, String>;

    /// Run `n` tool-call probes; return how many issued the expected call.
    async fn tool_call_probe(&self, n: u32) -> Result<u32, String>;

    /// Run the small coding eval (`n` tasks); return how many passed.
    async fn coding_eval(&self, n: u32) -> Result<u32, String>;
}

/// The result of a bench run: the measured [`LocalBench`] and the probed
/// [`ModelCapabilities`] (the first-use capability probe), ready to persist.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchOutcome {
    pub model_id: ModelId,
    pub local_bench: LocalBench,
    pub capabilities: ModelCapabilities,
}

impl BenchOutcome {
    /// Assemble a [`ModelProfile`] from this outcome for persistence, at the
    /// caller-supplied [`ModelLocation`].
    ///
    /// **`location` is load-bearing for security and must be derived, never
    /// assumed.** The routing hard filter keys off `is_local()` — a hosted model
    /// wrongly stamped `Local` short-circuits the classification filter and
    /// becomes eligible for *any* data. The caller derives it from the endpoint
    /// ([`endpoint_location`], fail-closed to [`ModelLocation::Hosted`] for a
    /// non-local or unparseable `base_url`); this method never picks a default.
    ///
    /// The performance seed is honest and minimal — reliability is the measured
    /// coding-eval score (the best single objective signal the bench produces),
    /// median latency is the measured time-to-first-token plus one generation, and
    /// `cost_per_1k_tokens_usd` is `0` (this is the *local*-bench harness; a hosted
    /// endpoint's real token price is not something the harness measures — the CLI
    /// warns when it benches a non-local endpoint). Observed per-task-class success
    /// accrues later from eval + trace data; this is the bench-time baseline.
    #[must_use]
    pub fn into_profile(self, location: ModelLocation) -> ModelProfile {
        let latency_ms_p50 = self.local_bench.time_to_first_token_ms
            + generation_ms(self.local_bench.tokens_per_second);
        ModelProfile {
            id: self.model_id,
            location,
            capabilities: self.capabilities,
            performance: ModelPerformance {
                reliability: self.local_bench.coding_eval_score,
                cost_per_1k_tokens_usd: 0.0,
                latency_ms_p50,
                task_class_success: Default::default(),
                failure_patterns: Vec::new(),
            },
            execution: ModelExecutionProfile::default(),
            bench: Some(self.local_bench),
        }
    }
}

/// Classify an endpoint `base_url` as [`ModelLocation::Local`] or `Hosted` for
/// the routing security filter (STEP 7.2). **Fail closed:** only a loopback
/// (`localhost`, `127.0.0.0/8`, `::1`) or RFC-1918 private-range host
/// (`10/8`, `172.16/12`, `192.168/16`) is `Local`; every other host — a public
/// IP, a domain, or a `base_url` that cannot be parsed — is `Hosted`, so a cloud
/// endpoint can never be persisted as local and slip past the classification
/// hard filter.
#[must_use]
pub fn endpoint_location(base_url: &str) -> ModelLocation {
    match host_from_base_url(base_url) {
        Some(host) if is_local_host(host) => ModelLocation::Local,
        _ => ModelLocation::Hosted,
    }
}

/// The bare host of a `scheme://[user@]host[:port]/path` URL (userinfo and port
/// stripped, IPv6 brackets removed), or `None` when no host is present.
fn host_from_base_url(base_url: &str) -> Option<&str> {
    let rest = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Drop any `user[:pass]@` userinfo.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    if authority.is_empty() {
        return None;
    }
    // Bracketed IPv6 literal: the host is between the brackets, port follows `]`.
    if let Some(after) = authority.strip_prefix('[') {
        return after.split(']').next().filter(|h| !h.is_empty());
    }
    // `host[:port]`: strip a trailing numeric port only.
    let host = authority.rsplit_once(':').map_or(authority, |(h, port)| {
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            authority
        }
    });
    (!host.is_empty()).then_some(host)
}

/// Whether `host` is a loopback or private-range address (or `localhost`).
fn is_local_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}

/// The wall-clock cost of generating a representative response at
/// `tokens_per_second` (a nominal 256-token response), for the latency seed. A
/// zero/negative rate degrades to `0` rather than dividing by zero.
fn generation_ms(tokens_per_second: f64) -> f64 {
    if tokens_per_second > 0.0 {
        256.0 / tokens_per_second * 1000.0
    } else {
        0.0
    }
}

/// Run the full bench against `target`, producing measured numbers only.
///
/// Warm-up is the cold/warm total-time delta (clamped at `0`); tokens/sec and
/// time-to-first-token come from the WARM generation (steady state); the three
/// scripted-probe scores are pass-fractions. A probe with a zero count scores
/// `0.0` (nothing demonstrated), never a fabricated `1.0`.
pub async fn run_bench(
    target: &dyn BenchTarget,
    options: BenchOptions,
) -> Result<BenchOutcome, String> {
    let description = target.describe().await?;
    let cold = target.timed_generation(false).await?;
    let warm = target.timed_generation(true).await?;

    let tokens_per_second = if warm.total.as_secs_f64() > 0.0 {
        warm.tokens as f64 / warm.total.as_secs_f64()
    } else {
        0.0
    };
    let warmup_ms = (cold.total.as_secs_f64() - warm.total.as_secs_f64()).max(0.0) * 1000.0;

    let structured = target
        .structured_output_probe(options.structured_output_probes)
        .await?;
    let tools = target.tool_call_probe(options.tool_call_probes).await?;
    let coding = target.coding_eval(options.coding_eval_tasks).await?;

    let local_bench = LocalBench {
        tokens_per_second,
        time_to_first_token_ms: warm.time_to_first_token.as_secs_f64() * 1000.0,
        warmup_ms,
        memory_mb: description.memory_mb,
        context_limit: description.context_limit,
        structured_output_reliability: fraction(structured, options.structured_output_probes),
        tool_call_accuracy: fraction(tools, options.tool_call_probes),
        coding_eval_score: fraction(coding, options.coding_eval_tasks),
    };
    Ok(BenchOutcome {
        model_id: target.model_id(),
        local_bench,
        capabilities: description.capabilities,
    })
}

/// `passes/total` clamped to `[0,1]`; `0.0` when `total` is `0` (nothing was
/// demonstrated — never a fabricated pass).
fn fraction(passes: u32, total: u32) -> f64 {
    if total == 0 {
        0.0
    } else {
        (f64::from(passes) / f64::from(total)).clamp(0.0, 1.0)
    }
}

/// A production [`BenchTarget`] wrapping any [`ModelDriver`]: measures what the
/// model-driver seam honestly surfaces. **Seam-limited (documented, not
/// fabricated):** the [`ModelDriver`] trait returns a whole [`ModelStep`] with no
/// streaming or provider token usage, so time-to-first-token equals total
/// generation time and token counts are estimated from the response text
/// (~4 bytes/token). The scripted probes are real: each poses a fixed prompt and
/// inspects the model's actual [`ModelStep`] (a `CallTool` for a tool probe,
/// JSON-parseable `Say` text for a structured-output probe, a keyword match for a
/// coding task). Richer numbers await provider usage plumbed through the driver
/// seam (the same gap the agent loop's `ModelRequestTrace` notes).
pub struct DriverBenchTarget<'a> {
    driver: &'a dyn ModelDriver,
    /// The capabilities/context/memory the endpoint config advertises — the
    /// driver seam does not surface these, so the caller supplies what it knows.
    description: TargetDescription,
}

impl<'a> DriverBenchTarget<'a> {
    /// Wrap `driver`, taking the advertised `description` from the endpoint config.
    #[must_use]
    pub fn new(driver: &'a dyn ModelDriver, description: TargetDescription) -> Self {
        Self {
            driver,
            description,
        }
    }

    /// One scripted probe: send `prompt` and return the model's step. The bench
    /// estimates tokens from the response text (the driver seam surfaces no usage
    /// for the bench's purposes), so the request's measured usage is discarded.
    async fn step(&self, prompt: &str) -> Result<ModelStep, String> {
        // The bench measures total generation time and the final `ModelStep`,
        // not streamed text, so any chunks the driver pushes are discarded.
        self.driver
            .next_step(
                &[TurnItem::Objective(prompt.to_string())],
                &mut NullDeltaSink,
            )
            .await
            .map(|outcome| outcome.step)
            .map_err(|e| format!("model driver error: {e}"))
    }
}

#[async_trait]
impl BenchTarget for DriverBenchTarget<'_> {
    fn model_id(&self) -> ModelId {
        self.driver.model_id()
    }

    async fn describe(&self) -> Result<TargetDescription, String> {
        Ok(self.description.clone())
    }

    async fn timed_generation(&self, _warm: bool) -> Result<GenerationSample, String> {
        let started = std::time::Instant::now();
        let step = self.step("Write one short sentence about routing.").await?;
        let total = started.elapsed();
        // Estimate tokens from the produced text (~4 bytes/token) — the driver
        // seam surfaces no usage. TTFT == total (no streaming through the seam).
        let text_len = match step {
            ModelStep::Say(text) => text.len(),
            ModelStep::Finish { summary } => summary.len(),
            ModelStep::CallTool { .. } => 0,
        };
        let tokens = (text_len as u64 / 4).max(1);
        Ok(GenerationSample {
            tokens,
            time_to_first_token: total,
            total,
        })
    }

    async fn structured_output_probe(&self, n: u32) -> Result<u32, String> {
        let mut passes = 0;
        for _ in 0..n {
            let step = self
                .step("Reply with a JSON object: {\"ok\": true}. JSON only.")
                .await?;
            if let ModelStep::Say(text) = step {
                if serde_json::from_str::<serde_json::Value>(text.trim()).is_ok() {
                    passes += 1;
                }
            }
        }
        Ok(passes)
    }

    async fn tool_call_probe(&self, n: u32) -> Result<u32, String> {
        let mut passes = 0;
        for _ in 0..n {
            let step = self
                .step("Read the file README.md using the workspace.read_file tool.")
                .await?;
            if matches!(step, ModelStep::CallTool { .. }) {
                passes += 1;
            }
        }
        Ok(passes)
    }

    async fn coding_eval(&self, n: u32) -> Result<u32, String> {
        let mut passes = 0;
        for _ in 0..n {
            let step = self
                .step("In Rust, what keyword declares an immutable binding? Answer with the keyword only.")
                .await?;
            let text = match step {
                ModelStep::Say(t) => t,
                ModelStep::Finish { summary } => summary,
                ModelStep::CallTool { .. } => String::new(),
            };
            if text.to_ascii_lowercase().contains("let") {
                passes += 1;
            }
        }
        Ok(passes)
    }
}

/// A fully scripted [`BenchTarget`] for tests: it returns the numbers it was
/// built with, so the whole harness (timing math, warm-up delta, probe
/// fractions, capability probe) runs with no model or network. Advertise or deny
/// features per scenario via [`Self::capabilities`].
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct MockBenchTarget {
    pub model_id: ModelId,
    pub capabilities: ModelCapabilities,
    pub context_limit: u64,
    pub memory_mb: u64,
    pub cold: GenerationSample,
    pub warm: GenerationSample,
    pub structured_passes: u32,
    pub tool_passes: u32,
    pub coding_passes: u32,
}

#[cfg(test)]
#[async_trait]
impl BenchTarget for MockBenchTarget {
    fn model_id(&self) -> ModelId {
        self.model_id.clone()
    }

    async fn describe(&self) -> Result<TargetDescription, String> {
        Ok(TargetDescription {
            capabilities: self.capabilities,
            context_limit: self.context_limit,
            memory_mb: self.memory_mb,
        })
    }

    async fn timed_generation(&self, warm: bool) -> Result<GenerationSample, String> {
        Ok(if warm {
            self.warm.clone()
        } else {
            self.cold.clone()
        })
    }

    async fn structured_output_probe(&self, n: u32) -> Result<u32, String> {
        Ok(self.structured_passes.min(n))
    }

    async fn tool_call_probe(&self, n: u32) -> Result<u32, String> {
        Ok(self.tool_passes.min(n))
    }

    async fn coding_eval(&self, n: u32) -> Result<u32, String> {
        Ok(self.coding_passes.min(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_routing::{StructuredOutputSupport, ToolCallSupport};

    fn caps(streaming: bool, tools: ToolCallSupport) -> ModelCapabilities {
        ModelCapabilities {
            streaming,
            tools,
            parallel_tools: matches!(tools, ToolCallSupport::Parallel),
            structured_output: StructuredOutputSupport::JsonMode,
            vision: false,
            audio_input: false,
            embeddings: false,
            prompt_caching: false,
            reasoning_controls: false,
            context_tokens: Some(128_000),
            output_tokens: Some(8_192),
        }
    }

    fn mock() -> MockBenchTarget {
        MockBenchTarget {
            model_id: ModelId("qwen-local".into()),
            capabilities: caps(true, ToolCallSupport::Parallel),
            context_limit: 128_000,
            memory_mb: 9_200,
            // A cold call is slower than the warm one (warm-up cost).
            cold: GenerationSample {
                tokens: 100,
                time_to_first_token: Duration::from_millis(400),
                total: Duration::from_millis(2_500),
            },
            warm: GenerationSample {
                tokens: 100,
                time_to_first_token: Duration::from_millis(180),
                total: Duration::from_millis(2_000),
            },
            structured_passes: 8,
            tool_passes: 7,
            coding_passes: 6,
        }
    }

    #[tokio::test]
    async fn bench_output_shape_is_measured_from_the_target() {
        let outcome = run_bench(&mock(), BenchOptions::default()).await.unwrap();
        let b = &outcome.local_bench;
        // 100 tokens over 2.0s warm = 50 tok/s.
        assert!((b.tokens_per_second - 50.0).abs() < 1e-6);
        assert!((b.time_to_first_token_ms - 180.0).abs() < 1e-6);
        // Warm-up = cold.total - warm.total = 2500 - 2000 = 500ms.
        assert!((b.warmup_ms - 500.0).abs() < 1e-6);
        assert_eq!(b.memory_mb, 9_200);
        assert_eq!(b.context_limit, 128_000);
        // Probe fractions: 8/10, 7/10, 6/10.
        assert!((b.structured_output_reliability - 0.8).abs() < 1e-9);
        assert!((b.tool_call_accuracy - 0.7).abs() < 1e-9);
        assert!((b.coding_eval_score - 0.6).abs() < 1e-9);
        assert_eq!(outcome.model_id, ModelId("qwen-local".into()));
    }

    #[tokio::test]
    async fn into_profile_uses_the_supplied_location_never_a_default() {
        let outcome = run_bench(&mock(), BenchOptions::default()).await.unwrap();
        // Local when told local...
        let local = outcome.clone().into_profile(ModelLocation::Local);
        assert!(local.is_local());
        assert_eq!(local.performance.cost_per_1k_tokens_usd, 0.0);
        assert!((local.performance.reliability - 0.6).abs() < 1e-9);
        assert_eq!(local.bench.as_ref().unwrap().tokens_per_second, 50.0);
        // ...and Hosted when told hosted — no hardcoded `Local` to leak past the filter.
        let hosted = outcome.into_profile(ModelLocation::Hosted);
        assert!(!hosted.is_local());
    }

    #[test]
    fn endpoint_location_is_local_only_for_loopback_and_private_hosts() {
        for local in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:11434/v1",
            "http://10.0.0.5:1234/v1",
            "http://192.168.1.9/v1",
            "http://172.16.4.2:9000/v1",
        ] {
            assert_eq!(endpoint_location(local), ModelLocation::Local, "{local}");
        }
        // Fail closed: public IPs, domains, and anything unparseable are Hosted —
        // a cloud endpoint can never be stored as local.
        for hosted in [
            "https://api.openai.com/v1",
            "https://192.168.1.9.evil.com/v1", // the host is a domain, not the IP
            "http://8.8.8.8/v1",
            "https://together.xyz/v1",
            "not-a-url",
            "",
        ] {
            assert_eq!(endpoint_location(hosted), ModelLocation::Hosted, "{hosted}");
        }
    }

    #[tokio::test]
    async fn the_capability_probe_reflects_the_scenario_advertised_features() {
        // Advertised: streaming + parallel tools.
        let advertised = run_bench(&mock(), BenchOptions::default()).await.unwrap();
        assert!(advertised.capabilities.streaming);
        assert_eq!(advertised.capabilities.tools, ToolCallSupport::Parallel);

        // Denied: a scenario whose target advertises no streaming and no tools.
        let mut denied_target = mock();
        denied_target.capabilities = caps(false, ToolCallSupport::None);
        let denied = run_bench(&denied_target, BenchOptions::default())
            .await
            .unwrap();
        assert!(!denied.capabilities.streaming);
        assert_eq!(denied.capabilities.tools, ToolCallSupport::None);
    }

    #[tokio::test]
    async fn a_zero_probe_count_scores_zero_never_a_fabricated_pass() {
        let outcome = run_bench(
            &mock(),
            BenchOptions {
                structured_output_probes: 0,
                tool_call_probes: 0,
                coding_eval_tasks: 0,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.local_bench.structured_output_reliability, 0.0);
        assert_eq!(outcome.local_bench.tool_call_accuracy, 0.0);
        assert_eq!(outcome.local_bench.coding_eval_score, 0.0);
    }

    #[tokio::test]
    async fn driver_bench_target_measures_over_the_model_driver_seam() {
        use crate::agent::{ModelStep, ScriptedDriver};
        // A scripted driver that calls a tool then finishes — the tool-call probe
        // should see a CallTool step.
        let driver = ScriptedDriver::new(vec![ModelStep::CallTool {
            tool: "workspace.read_file".into(),
            args: serde_json::json!({"path": "README.md"}),
        }])
        .with_model(ModelId("scripted-local".into()));
        let target = DriverBenchTarget::new(
            &driver,
            TargetDescription {
                capabilities: caps(true, ToolCallSupport::Single),
                context_limit: 32_000,
                memory_mb: 0,
            },
        );
        // One tool-call probe: the scripted first step is a CallTool, so it passes;
        // the driver then drains to Finish, so a second probe would not — we run one.
        let passes = target.tool_call_probe(1).await.unwrap();
        assert_eq!(passes, 1, "the probe sees the model's real CallTool step");
        assert_eq!(target.model_id(), ModelId("scripted-local".into()));
    }
}
