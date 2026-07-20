//! Model capability model (STEP 7.2): the [Chapter 09](../../docs/docs/09-model-routing-and-compaction.md)
//! `ModelCapabilities`, plus what a task node *requires* of a model.
//!
//! Capabilities are a **hard filter** in the routing pipeline — a model that
//! cannot call tools cannot serve a node that needs tool calls, regardless of how
//! cheap or fast it is. Requirements and capabilities meet in
//! [`ModelCapabilities::satisfies`], evaluated before any utility scoring.

use serde::{Deserialize, Serialize};

/// How well a model calls tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCallSupport {
    /// No tool calling.
    None,
    /// One tool call at a time.
    Single,
    /// Multiple tool calls in one turn.
    Parallel,
}

impl ToolCallSupport {
    #[must_use]
    pub fn can_call_tools(self) -> bool {
        !matches!(self, ToolCallSupport::None)
    }
    #[must_use]
    pub fn can_call_parallel(self) -> bool {
        matches!(self, ToolCallSupport::Parallel)
    }
}

/// How reliably a model produces structured (schema-constrained) output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StructuredOutputSupport {
    /// None — free text only.
    None,
    /// Best-effort JSON (may need schema repair).
    JsonMode,
    /// Constrained decoding — schema-valid output guaranteed.
    Strict,
}

impl StructuredOutputSupport {
    #[must_use]
    pub fn is_supported(self) -> bool {
        !matches!(self, StructuredOutputSupport::None)
    }
}

/// A model's declared capabilities (the Chapter 09 shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tools: ToolCallSupport,
    pub parallel_tools: bool,
    pub structured_output: StructuredOutputSupport,
    pub vision: bool,
    pub audio_input: bool,
    pub embeddings: bool,
    pub prompt_caching: bool,
    pub reasoning_controls: bool,
    pub context_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl ModelCapabilities {
    /// Whether this model meets a task node's `required` capabilities. This is the
    /// hard filter: a `false` here removes the model from consideration before
    /// utility is ever computed.
    #[must_use]
    pub fn satisfies(&self, required: &RequiredCapabilities) -> bool {
        if required.tools && !self.tools.can_call_tools() {
            return false;
        }
        if required.parallel_tools && !(self.parallel_tools && self.tools.can_call_parallel()) {
            return false;
        }
        if required.structured_output && !self.structured_output.is_supported() {
            return false;
        }
        if required.vision && !self.vision {
            return false;
        }
        if required.audio_input && !self.audio_input {
            return false;
        }
        // A model with an unknown context/output limit is assumed sufficient
        // (declared `None` means "unbounded / not advertised"); a declared limit
        // must fit the node's estimate.
        if let Some(ctx) = self.context_tokens {
            if required.min_context_tokens > ctx {
                return false;
            }
        }
        if let Some(out) = self.output_tokens {
            if required.min_output_tokens > out {
                return false;
            }
        }
        true
    }
}

/// What a task node requires of a model. Defaults to "no special requirements",
/// so a plain text task routes to any model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RequiredCapabilities {
    pub tools: bool,
    pub parallel_tools: bool,
    pub structured_output: bool,
    pub vision: bool,
    pub audio_input: bool,
    /// The context window the node's input needs to fit in.
    pub min_context_tokens: u64,
    /// The output length the node needs room for.
    pub min_output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tools: ToolCallSupport::Parallel,
            parallel_tools: true,
            structured_output: StructuredOutputSupport::Strict,
            vision: false,
            audio_input: false,
            embeddings: false,
            prompt_caching: true,
            reasoning_controls: true,
            context_tokens: Some(200_000),
            output_tokens: Some(8_192),
        }
    }

    #[test]
    fn satisfies_when_all_requirements_met() {
        let req = RequiredCapabilities {
            tools: true,
            parallel_tools: true,
            structured_output: true,
            min_context_tokens: 50_000,
            min_output_tokens: 4_000,
            ..Default::default()
        };
        assert!(caps().satisfies(&req));
    }

    #[test]
    fn tool_requirement_filters_a_no_tool_model() {
        let mut c = caps();
        c.tools = ToolCallSupport::None;
        let req = RequiredCapabilities {
            tools: true,
            ..Default::default()
        };
        assert!(!c.satisfies(&req));
    }

    #[test]
    fn parallel_requirement_filters_a_single_tool_model() {
        let mut c = caps();
        c.tools = ToolCallSupport::Single;
        let req = RequiredCapabilities {
            tools: true,
            parallel_tools: true,
            ..Default::default()
        };
        assert!(!c.satisfies(&req));
    }

    #[test]
    fn vision_requirement_filters_a_text_only_model() {
        let req = RequiredCapabilities {
            vision: true,
            ..Default::default()
        };
        assert!(!caps().satisfies(&req));
    }

    #[test]
    fn context_limit_filters_when_input_too_large() {
        let req = RequiredCapabilities {
            min_context_tokens: 500_000,
            ..Default::default()
        };
        assert!(!caps().satisfies(&req), "200k model can't fit a 500k task");
    }

    #[test]
    fn unbounded_context_always_fits() {
        let mut c = caps();
        c.context_tokens = None;
        let req = RequiredCapabilities {
            min_context_tokens: 10_000_000,
            ..Default::default()
        };
        assert!(c.satisfies(&req));
    }
}
