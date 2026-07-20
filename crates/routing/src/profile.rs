//! Model profiles (STEP 7.2): declared capabilities + **measured** performance +
//! the model-specific execution profile.
//!
//! Chapter 09 is explicit that routing reads *measured* numbers, never vibes: a
//! [`ModelProfile`] carries declared [`ModelCapabilities`](crate::capability::ModelCapabilities),
//! observed [`ModelPerformance`] (reliability, cost/latency distributions,
//! per-task-class success), a [`ModelExecutionProfile`] (how to *drive* the model
//! — preferred tool count, edit protocol, reasoning budget, schema repair), and,
//! for local models, a [`LocalBench`] of the harness-measured profile
//! (`codypendent models bench <id>`). The router consumes these; nothing here
//! makes a network call.

use std::collections::BTreeMap;

use codypendent_protocol::ids::ModelId;
use serde::{Deserialize, Serialize};

use crate::capability::ModelCapabilities;
use crate::classify::TaskClass;

/// Where a model runs — the privacy-relevant distinction the routing hard filter
/// uses (local models can process any data classification; hosted models are
/// gated by policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelLocation {
    /// On-device (embedded, subprocess, or LAN service treated as local by policy).
    Local,
    /// Off-device (a hosted/cloud provider).
    Hosted,
}

impl ModelLocation {
    #[must_use]
    pub fn is_local(self) -> bool {
        matches!(self, ModelLocation::Local)
    }
}

/// A model's observed performance — the measured inputs to routing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPerformance {
    /// Overall observed success rate `[0,1]` (the baseline when a task class has
    /// no history yet).
    pub reliability: f64,
    /// Blended cost per 1K tokens, in USD (input+output amortized).
    pub cost_per_1k_tokens_usd: f64,
    /// Median end-to-end latency, in milliseconds.
    pub latency_ms_p50: f64,
    /// Per-task-class observed success rate `[0,1]` (from eval + trace data).
    #[serde(default)]
    pub task_class_success: BTreeMap<String, f64>,
    /// Known failure-pattern tags (e.g. `schema-drift`, `tool-loop`) — advisory
    /// context for graders/telemetry, not a routing filter.
    #[serde(default)]
    pub failure_patterns: Vec<String>,
}

impl ModelPerformance {
    /// The predicted success for a task class: the class-specific observed rate if
    /// one exists, else the overall reliability. This is the utility's
    /// `predicted_success` term and the quality-threshold gate.
    #[must_use]
    pub fn predicted_success(&self, class: TaskClass) -> f64 {
        self.task_class_success
            .get(class.as_str())
            .copied()
            .unwrap_or(self.reliability)
    }

    /// Failure probability = `1 - predicted_success`.
    #[must_use]
    pub fn failure_probability(&self, class: TaskClass) -> f64 {
        1.0 - self.predicted_success(class)
    }
}

/// The edit protocol a model is driven with (evaluation-derived, Chapter 09).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditProtocol {
    /// Structured patch/diff tool calls.
    StructuredPatch,
    /// Whole-file rewrites.
    WholeFile,
    /// Architect/implementer separation (plan then apply).
    ArchitectImplementer,
}

/// How to drive a specific model (Chapter 09 `ModelExecutionProfile`). Preserved
/// across a mid-session switch so the runtime adapts to the new model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelExecutionProfile {
    /// How many tools to expose at once (some models degrade with too many).
    pub preferred_tool_count: usize,
    pub edit_protocol: EditProtocol,
    /// A free-form context-layout tag (ordering of system/context/history).
    pub context_layout: String,
    /// Reasoning-budget hint, when the model supports reasoning controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<u32>,
    /// What to do when structured output fails to parse.
    pub schema_repair: SchemaRepairPolicy,
}

impl Default for ModelExecutionProfile {
    fn default() -> Self {
        Self {
            preferred_tool_count: 8,
            edit_protocol: EditProtocol::StructuredPatch,
            context_layout: "system-context-history".to_string(),
            reasoning_budget: None,
            schema_repair: SchemaRepairPolicy::Reprompt,
        }
    }
}

/// What to do when a model returns output that fails schema validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SchemaRepairPolicy {
    /// Give up immediately (escalate).
    None,
    /// Re-prompt with the validation error.
    Reprompt,
    /// Attempt a local structural repair before re-prompting.
    LocalRepair,
}

/// The harness-measured local-model profile (`codypendent models bench <id>`).
/// Populated by STEP 7.2's benchmark harness; the router treats a local model's
/// measured `structured_output_reliability`/`tool_call_accuracy`/`coding_eval_score`
/// as authoritative over any declared capability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalBench {
    pub tokens_per_second: f64,
    pub time_to_first_token_ms: f64,
    pub warmup_ms: f64,
    pub memory_mb: u64,
    pub context_limit: u64,
    /// Reliability of structured output over scripted probes `[0,1]`.
    pub structured_output_reliability: f64,
    /// Tool-call accuracy over scripted probes `[0,1]`.
    pub tool_call_accuracy: f64,
    /// Score on the small coding eval (10 tasks) `[0,1]`.
    pub coding_eval_score: f64,
}

/// A complete model profile: identity, location, capabilities, measured
/// performance, execution profile, and (for local models) the bench.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    pub id: ModelId,
    pub location: ModelLocation,
    pub capabilities: ModelCapabilities,
    pub performance: ModelPerformance,
    pub execution: ModelExecutionProfile,
    /// Present for local models measured by the bench harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<LocalBench>,
}

impl ModelProfile {
    /// The expected cost, in USD, of running a node estimated at `total_tokens`.
    #[must_use]
    pub fn expected_cost_usd(&self, total_tokens: u64) -> f64 {
        self.performance.cost_per_1k_tokens_usd * (total_tokens as f64 / 1000.0)
    }

    #[must_use]
    pub fn is_local(&self) -> bool {
        self.location.is_local()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicted_success_prefers_task_class_history() {
        let mut perf = ModelPerformance {
            reliability: 0.7,
            cost_per_1k_tokens_usd: 0.01,
            latency_ms_p50: 800.0,
            task_class_success: BTreeMap::new(),
            failure_patterns: vec![],
        };
        // Without class history, falls back to overall reliability.
        assert_eq!(perf.predicted_success(TaskClass::DocUpdate), 0.7);
        // With class history, uses it.
        perf.task_class_success.insert("doc-update".into(), 0.95);
        assert_eq!(perf.predicted_success(TaskClass::DocUpdate), 0.95);
        assert!((perf.failure_probability(TaskClass::DocUpdate) - 0.05).abs() < 1e-9);
        // A different class still falls back.
        assert_eq!(perf.predicted_success(TaskClass::SmallBugFix), 0.7);
    }

    #[test]
    fn expected_cost_scales_with_tokens() {
        let profile = ModelProfile {
            id: ModelId("m".into()),
            location: ModelLocation::Hosted,
            capabilities: ModelCapabilities {
                streaming: true,
                tools: crate::capability::ToolCallSupport::Parallel,
                parallel_tools: true,
                structured_output: crate::capability::StructuredOutputSupport::Strict,
                vision: false,
                audio_input: false,
                embeddings: false,
                prompt_caching: true,
                reasoning_controls: false,
                context_tokens: Some(200_000),
                output_tokens: Some(8192),
            },
            performance: ModelPerformance {
                reliability: 0.9,
                cost_per_1k_tokens_usd: 0.02,
                latency_ms_p50: 500.0,
                task_class_success: BTreeMap::new(),
                failure_patterns: vec![],
            },
            execution: ModelExecutionProfile::default(),
            bench: None,
        };
        // 10K tokens at $0.02/1K = $0.20.
        assert!((profile.expected_cost_usd(10_000) - 0.20).abs() < 1e-9);
    }
}
