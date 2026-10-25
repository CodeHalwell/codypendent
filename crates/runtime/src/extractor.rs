//! The model-backed [`FactExtractor`] (M3b): a bounded, fallback-safe LLM call
//! that distils a finished run into discrete facts.
//!
//! This is the ONE place in the smarter-memory feature that calls a model —
//! everywhere else (`crates/knowledge`) stays model-free (ADR-009: only this
//! crate depends on `agent-framework-rs`, and only behind provider features).
//! Every bound here is enforced on the way IN (input tail-cap) and on the way
//! OUT (fact count, statement length) so a misbehaving model can degrade the
//! quality of what is remembered but can never blow the run's budget or fail
//! it: [`FactExtractor::extract`] returns `Vec<CandidateMemory>`, never
//! `Result`, and this impl's `extract` wraps its ENTIRE inner call — prompt
//! build, network round trip, stream drain, parse — in
//! `tokio::time::timeout`, converting ANY error, any timeout, or an
//! empty/unparseable response into `vec![]` rather than propagating.

use std::sync::Arc;
use std::time::Duration;

use agent_framework_core::client::ChatClient;
use agent_framework_core::types::{ChatOptions, Message};
use codypendent_knowledge::{
    CandidateMemory, EvidenceRef, ExtractionInput, FactExtractor, MemoryClass,
};
use codypendent_protocol::ModelId;
use futures::StreamExt;

use crate::models::ModelRegistry;

// ---------------------------------------------------------------------------
// D2 bounds
// ---------------------------------------------------------------------------

/// Input budget (D2): the run context handed to the model is capped to the
/// LAST this-many chars (the tail — the most recent, most relevant context)
/// before it is ever sent.
const LLM_EXTRACT_INPUT_MAX: usize = 32_000;
/// Output budget (D2): at most this many discrete facts are kept from a
/// single extraction, regardless of how many the model returns.
const LLM_EXTRACT_MAX_FACTS: usize = 10;
/// Output budget (D2): each fact's `statement` is capped to this many chars —
/// short and standalone, per the one-line-per-fact contract.
const LLM_EXTRACT_STATEMENT_MAX: usize = 200;
/// Time budget (D2): the whole model call (prompt build through stream
/// drain) is bounded by this timeout; exceeding it contributes nothing.
const LLM_EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);

/// The confidence assigned to a fact whose `confidence` field is absent from
/// the model's response — a neutral default, distinct from a model that
/// explicitly reported low or high confidence.
const DEFAULT_FACT_CONFIDENCE: f32 = 0.6;

const SYSTEM_PROMPT: &str = "Extract at most 10 discrete, standalone facts worth \
    remembering across future runs, as a JSON array of objects shaped \
    {\"kind\": string, \"statement\": string, \"evidence\": string (optional), \
    \"confidence\": number 0..1 (optional)}. `kind` must be one of: finding, \
    decision, learning, failure, preference. Each `statement` must stand alone \
    (understandable without the rest of the run) and be under 200 characters. \
    Respond with ONLY the JSON array — no prose, no markdown fences, nothing \
    outside the array.";

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Keep the LAST `max` chars of `s`, on a UTF-8 char boundary. Returns `s`
/// unchanged when it already fits. Pure — no allocation beyond the borrow.
///
/// The tail (not the head) is kept deliberately: the most recent transcript
/// context is usually the most relevant to what just happened, and a run's
/// transcript is built in chronological order, so the tail is also the most
/// recently written part.
#[must_use]
fn tail_cap(s: &str, max: usize) -> &str {
    let total_chars = s.chars().count();
    if total_chars <= max {
        return s;
    }
    let skip = total_chars - max;
    match s.char_indices().nth(skip) {
        Some((idx, _)) => &s[idx..],
        None => s,
    }
}

/// Truncate `s` to at most `max` chars on a char boundary, appending `…` only
/// when it actually cut. Mirrors `codypendent_knowledge::observer::cap_chars`
/// (not reusable directly — that helper is private to its module), kept here
/// so a fact's `statement` is bounded independent of what the model sent.
#[must_use]
fn cap_statement(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some(_) => {
            let cut_idx = s.char_indices().nth(max.saturating_sub(1)).map_or(0, |(idx, _)| idx);
            let mut truncated = String::with_capacity(cut_idx + 3);
            truncated.push_str(&s[..cut_idx]);
            truncated.push('\u{2026}');
            truncated
        }
        None => s.to_string(),
    }
}

/// Map a model-reported fact `kind` to a [`MemoryClass`] (D2 mapping table).
/// An unrecognized/missing kind defaults to `Semantic` — the same bucket as
/// `finding`/`fact`/`decision` — rather than being dropped, since an
/// unfamiliar label is still a plausible discrete fact worth keeping.
#[must_use]
fn kind_to_class(kind: &str) -> MemoryClass {
    match kind.to_ascii_lowercase().as_str() {
        "learning" | "procedure" => MemoryClass::Procedural,
        "failure" | "pitfall" => MemoryClass::Failure,
        "preference" => MemoryClass::Preference,
        // "finding" | "fact" | "decision" | unknown
        _ => MemoryClass::Semantic,
    }
}

/// Parse the model's raw response `text` (expected: a JSON array of
/// `{kind, statement, evidence?, confidence?}`) into bounded
/// [`CandidateMemory`]s, inheriting scope/observed_at/valid_from/sensitivity
/// from `input` and citing `input.chronicle_ref` as every fact's provenance
/// (so a fact is never evidence-free entering `curate`'s gate (e)).
///
/// Defensive by construction: non-JSON, a JSON value that is not an array, or
/// an entry missing/blank `statement` never panics — it is simply skipped
/// (or, for the whole-text case, yields `vec![]`). Output is capped at
/// [`LLM_EXTRACT_MAX_FACTS`]; each `statement` is capped at
/// [`LLM_EXTRACT_STATEMENT_MAX`]; `confidence` is clamped to `[0, 1]`,
/// defaulting to [`DEFAULT_FACT_CONFIDENCE`] when absent or non-numeric.
#[must_use]
fn parse_facts(text: &str, input: &ExtractionInput<'_>) -> Vec<CandidateMemory> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return Vec::new();
    };
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries {
        if out.len() >= LLM_EXTRACT_MAX_FACTS {
            break;
        }
        let Some(raw_statement) = entry.get("statement").and_then(|v| v.as_str()) else {
            continue;
        };
        let trimmed = raw_statement.trim();
        if trimmed.is_empty() {
            continue;
        }
        let statement = cap_statement(trimmed, LLM_EXTRACT_STATEMENT_MAX);
        let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let confidence = entry
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .map_or(DEFAULT_FACT_CONFIDENCE, |c| c.clamp(0.0, 1.0) as f32);

        out.push(CandidateMemory {
            class: kind_to_class(kind),
            scope: Some(input.scope.clone()),
            statement,
            structured_value: None,
            provenance: vec![EvidenceRef::Artifact {
                artifact: input.chronicle_ref.clone(),
                source_path: None,
            }],
            confidence,
            observed_at: input.observed_at,
            valid_from: input.valid_from.clone(),
            sensitivity: input.sensitivity,
            retention: None,
        });
    }
    out
}

/// Build the two-message prompt (system instructions + a user turn carrying
/// the objective, the compact chronicle, and the tail-capped transcript).
/// Pure and unit-testable independent of any live client.
#[must_use]
fn build_messages(input: &ExtractionInput<'_>) -> Vec<Message> {
    let compact_chronicle = serde_json::to_string(input.chronicle).unwrap_or_default();
    let bounded_transcript = tail_cap(input.transcript_excerpt, LLM_EXTRACT_INPUT_MAX);
    let user_text = format!(
        "Objective: {}\n\nRun chronicle (JSON): {}\n\nTranscript excerpt (most recent last):\n{}",
        input.objective, compact_chronicle, bounded_transcript
    );
    vec![Message::system(SYSTEM_PROMPT), Message::user(user_text)]
}

// ---------------------------------------------------------------------------
// LlmFactExtractor
// ---------------------------------------------------------------------------

/// A [`FactExtractor`] backed by a real model call through the framework
/// [`ChatClient`]. Built via [`LlmFactExtractor::from_registry`] (mirrors
/// [`crate::agent::FrameworkModelDriver::from_registry`]) from whichever
/// model the D2 selection order in `build_fact_extractor` resolved.
pub struct LlmFactExtractor {
    client: Arc<dyn ChatClient>,
    model_id: ModelId,
}

impl LlmFactExtractor {
    /// Wrap an already-built client and the model id it serves.
    #[must_use]
    pub fn new(client: Arc<dyn ChatClient>, model_id: ModelId) -> Self {
        Self { client, model_id }
    }

    /// Build an extractor from the registry by resolving `model_id` to a
    /// client. Fails when the model is unregistered or its client cannot be
    /// constructed (e.g. a missing API key env var) — the caller
    /// (`build_fact_extractor`) falls back to [`codypendent_knowledge::NoopExtractor`]
    /// on any `Err`, so a broken extraction config disables extraction for
    /// this run rather than failing it.
    pub async fn from_registry(models: &ModelRegistry, model_id: ModelId) -> anyhow::Result<Self> {
        let client = models
            .client_for(&model_id)
            .await
            .map_err(|e| anyhow::anyhow!("could not build client for {model_id}: {e}"))?;
        Ok(Self::new(client, model_id))
    }

    /// The live model round trip: build the prompt, call
    /// [`ChatClient::get_streaming_response`], and drain the stream into a
    /// single string (mirrors `FrameworkModelDriver::next_step`'s drain, but
    /// with no `DeltaSink` — extraction happens after the run is already
    /// finished, so there is nothing to stream live to). A mid-stream error
    /// propagates via `?`, which the caller (`extract_with_timeout`) treats
    /// exactly like any other failure: `vec![]`, never a panic or a failed
    /// run.
    async fn run(
        &self,
        input: &ExtractionInput<'_>,
    ) -> agent_framework_core::error::Result<String> {
        let messages = build_messages(input);
        let mut stream = self
            .client
            .get_streaming_response(messages, ChatOptions::new())
            .await?;

        let mut text = String::new();
        while let Some(update) = stream.next().await {
            text.push_str(&update?.text_content());
        }
        Ok(text)
    }

    /// The testable core: run the model call under `timeout`, converting ANY
    /// outcome other than "a well-formed fact array arrived in time" into
    /// `vec![]`. Exposed with an injectable timeout (rather than hard-coding
    /// [`LLM_EXTRACT_TIMEOUT`] here) purely so a timeout can be exercised in a
    /// fast unit test with a real (unpaused) clock; [`FactExtractor::extract`]
    /// always calls this with the real [`LLM_EXTRACT_TIMEOUT`].
    async fn extract_with_timeout(
        &self,
        input: &ExtractionInput<'_>,
        timeout: Duration,
    ) -> Vec<CandidateMemory> {
        match tokio::time::timeout(timeout, self.run(input)).await {
            Ok(Ok(text)) => {
                let facts = parse_facts(&text, input);
                if facts.is_empty() {
                    tracing::warn!(
                        model = %self.model_id,
                        "memory extraction returned no parseable facts; contributing none"
                    );
                }
                facts
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    model = %self.model_id, %error,
                    "memory extraction model call failed; contributing no facts"
                );
                Vec::new()
            }
            Err(_elapsed) => {
                tracing::warn!(
                    model = %self.model_id, ?timeout,
                    "memory extraction timed out; contributing no facts"
                );
                Vec::new()
            }
        }
    }
}

#[async_trait::async_trait]
impl FactExtractor for LlmFactExtractor {
    /// Total fallback (D2): ANY error, timeout, missing config, or
    /// empty/unparseable response yields `vec![]` — never a panic, never a
    /// propagated error. This is the ONE call site that matters for the
    /// "extraction can never fail a run" guarantee: everything inside
    /// `extract_with_timeout` that could go wrong already collapses to
    /// `Vec::new()` before it gets here.
    async fn extract(&self, input: ExtractionInput<'_>) -> Vec<CandidateMemory> {
        self.extract_with_timeout(&input, LLM_EXTRACT_TIMEOUT).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_framework_core::client::ChatStream;
    use agent_framework_core::error::{Error as FrameworkError, Result as FrameworkResult};
    use agent_framework_core::types::{ChatResponse, ChatResponseUpdate};
    use chrono::Utc;
    use codypendent_knowledge::{Revision, Scope};
    use codypendent_protocol::{ArtifactId, DataClassification, RunId};

    fn chronicle_ref() -> codypendent_protocol::ArtifactRef {
        codypendent_protocol::ArtifactRef {
            id: ArtifactId::new(),
            media_type: "application/json".to_string(),
            byte_length: 0,
            sha256: "0".repeat(64),
            sensitivity: DataClassification::Internal,
        }
    }

    fn minimal_input<'a>(
        chronicle: &'a serde_json::Value,
        scope: &'a Scope,
        chronicle_ref: &'a codypendent_protocol::ArtifactRef,
    ) -> ExtractionInput<'a> {
        ExtractionInput {
            objective: "fix the guard",
            chronicle,
            transcript_excerpt: "",
            scope,
            chronicle_ref,
            run_id: RunId::new(),
            observed_at: Utc::now(),
            valid_from: Revision::sequence(1),
            sensitivity: DataClassification::Internal,
        }
    }

    // -- tail_cap ----------------------------------------------------------

    #[test]
    fn tail_cap_returns_the_whole_string_when_under_the_limit() {
        assert_eq!(tail_cap("short", 100), "short");
    }

    #[test]
    fn tail_cap_keeps_the_last_max_chars() {
        let s = "0123456789";
        assert_eq!(tail_cap(s, 4), "6789");
    }

    #[test]
    fn tail_cap_is_char_boundary_safe_on_multibyte_input() {
        let s = "a".repeat(5) + "日本語のテスト";
        // Must not panic slicing mid-codepoint, and the result must still be
        // valid UTF-8 of the requested char length.
        let capped = tail_cap(&s, 5);
        assert_eq!(capped.chars().count(), 5);
    }

    #[test]
    fn tail_cap_enforces_the_32k_bound_on_a_larger_input() {
        let s = "x".repeat(LLM_EXTRACT_INPUT_MAX + 500);
        let capped = tail_cap(&s, LLM_EXTRACT_INPUT_MAX);
        assert_eq!(capped.chars().count(), LLM_EXTRACT_INPUT_MAX);
    }

    // -- kind_to_class -------------------------------------------------------

    #[test]
    fn kind_to_class_maps_the_d2_table() {
        assert_eq!(kind_to_class("finding"), MemoryClass::Semantic);
        assert_eq!(kind_to_class("fact"), MemoryClass::Semantic);
        assert_eq!(kind_to_class("decision"), MemoryClass::Semantic);
        assert_eq!(kind_to_class("unknown-kind"), MemoryClass::Semantic);
        assert_eq!(kind_to_class("learning"), MemoryClass::Procedural);
        assert_eq!(kind_to_class("procedure"), MemoryClass::Procedural);
        assert_eq!(kind_to_class("failure"), MemoryClass::Failure);
        assert_eq!(kind_to_class("pitfall"), MemoryClass::Failure);
        assert_eq!(kind_to_class("preference"), MemoryClass::Preference);
        // Case-insensitive.
        assert_eq!(kind_to_class("FAILURE"), MemoryClass::Failure);
    }

    // -- parse_facts ---------------------------------------------------------

    #[test]
    fn parse_facts_maps_a_well_formed_array() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let text = serde_json::json!([
            {"kind": "decision", "statement": "prefer sqlx over diesel", "confidence": 0.9},
            {"kind": "failure", "statement": "retrying without backoff floods the API"},
            {"kind": "preference", "statement": "wants short PR titles"},
        ])
        .to_string();

        let facts = parse_facts(&text, &input);
        assert_eq!(facts.len(), 3);
        assert_eq!(facts[0].class, MemoryClass::Semantic);
        assert_eq!(facts[0].statement, "prefer sqlx over diesel");
        assert_eq!(facts[0].confidence, 0.9);
        assert_eq!(facts[1].class, MemoryClass::Failure);
        // Absent confidence defaults to the neutral default.
        assert_eq!(facts[1].confidence, DEFAULT_FACT_CONFIDENCE);
        assert_eq!(facts[2].class, MemoryClass::Preference);

        // Every fact cites the chronicle artifact and inherits the input's
        // scope/observed_at/valid_from/sensitivity, with no retention override.
        for fact in &facts {
            assert_eq!(fact.scope, Some(scope.clone()));
            assert_eq!(fact.observed_at, input.observed_at);
            assert_eq!(fact.valid_from, input.valid_from);
            assert_eq!(fact.sensitivity, input.sensitivity);
            assert_eq!(fact.retention, None);
            assert_eq!(
                fact.provenance,
                vec![EvidenceRef::Artifact {
                    artifact: cref.clone(),
                    source_path: None,
                }]
            );
        }
    }

    #[test]
    fn parse_facts_clamps_confidence_into_zero_one() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let text = serde_json::json!([
            {"kind": "finding", "statement": "a", "confidence": 5.0},
            {"kind": "finding", "statement": "b", "confidence": -3.0},
        ])
        .to_string();

        let facts = parse_facts(&text, &input);
        assert_eq!(facts[0].confidence, 1.0);
        assert_eq!(facts[1].confidence, 0.0);
    }

    #[test]
    fn parse_facts_truncates_a_statement_over_200_chars() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let long_statement = "a".repeat(500);
        let text =
            serde_json::json!([{"kind": "finding", "statement": long_statement}]).to_string();

        let facts = parse_facts(&text, &input);
        assert_eq!(facts.len(), 1);
        assert!(facts[0].statement.chars().count() <= LLM_EXTRACT_STATEMENT_MAX);
        assert!(facts[0].statement.ends_with('\u{2026}'));
    }

    #[test]
    fn parse_facts_caps_output_at_ten_even_when_the_model_sent_more() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let entries: Vec<_> = (0..25)
            .map(|i| serde_json::json!({"kind": "finding", "statement": format!("fact {i}")}))
            .collect();
        let text = serde_json::Value::Array(entries).to_string();

        let facts = parse_facts(&text, &input);
        assert_eq!(facts.len(), LLM_EXTRACT_MAX_FACTS);
    }

    #[test]
    fn parse_facts_drops_empty_or_blank_statements() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let text = serde_json::json!([
            {"kind": "finding", "statement": ""},
            {"kind": "finding", "statement": "   "},
            {"kind": "finding", "statement": "a real fact"},
        ])
        .to_string();

        let facts = parse_facts(&text, &input);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].statement, "a real fact");
    }

    #[test]
    fn parse_facts_on_garbage_input_yields_nothing() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        assert!(parse_facts("not json at all", &input).is_empty());
        assert!(parse_facts("{\"not\": \"an array\"}", &input).is_empty());
        assert!(parse_facts("", &input).is_empty());
        assert!(parse_facts("[not, valid, json]", &input).is_empty());
    }

    #[test]
    fn parse_facts_skips_entries_missing_a_statement_field() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let text = serde_json::json!([
            {"kind": "finding"},
            {"kind": "finding", "statement": "kept"},
        ])
        .to_string();

        let facts = parse_facts(&text, &input);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].statement, "kept");
    }

    // -- LlmFactExtractor: mock ChatClient ------------------------------------

    /// A `ChatClient` test double that either streams back a fixed text
    /// response or fails, optionally after a delay — used to exercise the
    /// success/error/timeout paths of `extract`/`extract_with_timeout`
    /// without a live model.
    struct MockClient {
        outcome: MockOutcome,
        delay: Duration,
    }

    enum MockOutcome {
        Text(String),
        Error,
    }

    #[async_trait::async_trait]
    impl ChatClient for MockClient {
        async fn get_response(
            &self,
            _messages: Vec<Message>,
            _options: ChatOptions,
        ) -> FrameworkResult<ChatResponse> {
            Err(FrameworkError::Service(
                "get_response not used by extractor".to_string(),
            ))
        }

        async fn get_streaming_response(
            &self,
            _messages: Vec<Message>,
            _options: ChatOptions,
        ) -> FrameworkResult<ChatStream> {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            match &self.outcome {
                MockOutcome::Text(text) => {
                    let update = ChatResponseUpdate::text(text.clone());
                    Ok(Box::pin(futures::stream::iter(vec![Ok(update)])))
                }
                MockOutcome::Error => Err(FrameworkError::Service("mock failure".to_string())),
            }
        }
    }

    fn extractor_with(outcome: MockOutcome, delay: Duration) -> LlmFactExtractor {
        LlmFactExtractor::new(
            Arc::new(MockClient { outcome, delay }),
            ModelId("mock-model".to_string()),
        )
    }

    #[tokio::test]
    async fn extract_maps_a_well_formed_model_response_to_candidate_memories() {
        let chronicle = serde_json::json!({"objective": "fix the guard"});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let response = serde_json::json!([
            {"kind": "decision", "statement": "prefer sqlx over diesel", "confidence": 0.8},
            {"kind": "failure", "statement": "forgot to add backoff"},
        ])
        .to_string();
        let extractor = extractor_with(MockOutcome::Text(response), Duration::ZERO);

        let facts = extractor.extract(input).await;
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].class, MemoryClass::Semantic);
        assert_eq!(facts[1].class, MemoryClass::Failure);
        for fact in &facts {
            assert!(
                !fact.provenance.is_empty(),
                "every fact must carry provenance"
            );
        }
    }

    #[tokio::test]
    async fn extract_on_a_client_error_yields_no_facts() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let extractor = extractor_with(MockOutcome::Error, Duration::ZERO);
        let facts = extractor.extract(input).await;
        assert!(
            facts.is_empty(),
            "a client error must fall back to vec![], never panic"
        );
    }

    #[tokio::test]
    async fn extract_on_garbage_response_yields_no_facts() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let extractor = extractor_with(MockOutcome::Text("not json".to_string()), Duration::ZERO);
        let facts = extractor.extract(input).await;
        assert!(
            facts.is_empty(),
            "an unparseable response must fall back to vec![]"
        );
    }

    #[tokio::test]
    async fn extract_that_exceeds_the_timeout_yields_no_facts() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        // The mock client "answers" well past a short injected timeout — proves
        // `extract_with_timeout` converts an elapsed timeout into `vec![]`
        // exactly like any other failure, without waiting on the real 30s
        // production bound.
        let extractor = extractor_with(
            MockOutcome::Text(
                serde_json::json!([{"kind": "finding", "statement": "too slow"}]).to_string(),
            ),
            Duration::from_millis(200),
        );
        let facts = extractor
            .extract_with_timeout(&input, Duration::from_millis(20))
            .await;
        assert!(
            facts.is_empty(),
            "an elapsed timeout must fall back to vec![], never hang or panic"
        );
    }

    #[tokio::test]
    async fn extract_is_object_safe_as_a_fact_extractor() {
        let chronicle = serde_json::json!({});
        let scope = Scope::System;
        let cref = chronicle_ref();
        let input = minimal_input(&chronicle, &scope, &cref);

        let extractor = extractor_with(MockOutcome::Text("[]".to_string()), Duration::ZERO);
        let dyn_extractor: &dyn FactExtractor = &extractor;
        let facts = dyn_extractor.extract(input).await;
        assert!(facts.is_empty());
    }
}
