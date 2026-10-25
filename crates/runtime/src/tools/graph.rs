//! `graph.callers_of` / `graph.blast_radius` / `graph.tests_covering` — the
//! agent's window onto the repository's code graph (outcome 5) — and
//! `graph.assert_edge`, the agent's WRITE onto it.
//!
//! The graph has always been parsed, stored, and queryable; until now the model's
//! only exposure to it was the static `RepositoryMap` text folded once into a
//! run's opening note. It could not ask "who calls this", "what breaks if I
//! change this", or "which tests cover this file" — the three questions the store
//! was built to answer (2026-08-13 review, F3).
//!
//! # The two levers
//!
//! Graph construction has a deterministic lever (tree-sitter: what the parser can
//! see, folded on every scan) and, from here, an **agent** lever: a relation the
//! parser *cannot* see because it does not exist in the syntax — a route table
//! entry to the service it dispatches to, a config key to the code that reads it,
//! a test to the behaviour it covers, a migration to the model it reshapes. The
//! deterministic layer cannot express those; a model reading the code can.
//!
//! The two are kept apart by evidence, not by trust: an assertion is stored as
//! [`EvidenceKind::AgentAsserted`](codypendent_knowledge::EvidenceKind::AgentAsserted)
//! at its own confidence and supersedes only a *strictly less confident* edge, so
//! a model's claim can never displace a compiler- or LSP-resolved fact. The
//! engine enforces that (`codegraph::assert_agent_edges`); this module's job is
//! to make the outcome legible to the model that asked.
//!
//! Like `skills.search` and the MCP bridge, this module holds argument parsing,
//! the seam, and the rendering: the queries, their bounds, and the write engine
//! live in `codypendent_knowledge::codegraph`, and both seams
//! ([`CodeGraphQueries`](codypendent_knowledge::CodeGraphQueries) and
//! [`CodeGraphAssertions`]) are implemented in the daemon assembly, which owns
//! the pool. Without an injected implementation the tools are never offered.
//!
//! Every result is first-party: node names and paths this daemon parsed out of
//! the repository itself. There is no untrusted content to sanitize here, unlike
//! the MCP/web/skill-document arms — a symbol name is bounded by the parser, and
//! `GraphAnswer::render` caps the disclosed rows.

use std::path::Path;

use async_trait::async_trait;
use codypendent_knowledge::{CodeRelation, EvidenceKind, GraphQuestion};
use codypendent_protocol::{ProposedAction, RunId, SessionId};
use serde_json::Value;

/// Default traversal depth when a call omits it. Two layers is the useful answer
/// for "what breaks if I change this" without turning every call into a
/// whole-repository dump; the ceiling is enforced downstream.
pub const DEFAULT_DEPTH: usize = 2;

/// `graph.callers_of` — the direct callers of a symbol.
pub struct GraphCallersOf;
/// `graph.blast_radius` — everything that transitively reaches a symbol.
pub struct GraphBlastRadius;
/// `graph.tests_covering` — the tests that exercise a file.
pub struct GraphTestsCovering;

impl GraphCallersOf {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "graph.callers_of";
}
impl GraphBlastRadius {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "graph.blast_radius";
}
impl GraphTestsCovering {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "graph.tests_covering";
}

/// The action policy evaluates for any `graph.*` call: a READ of the daemon's
/// own derived code graph — no filesystem, command, network, or remote effect,
/// and no model-supplied path ever reaches the filesystem (the argument is
/// matched against `code_nodes.source_path` rows the daemon itself wrote).
/// Always policy-`Allow`ed, exactly like `SearchRegistry`, and recorded only so
/// the access is traced.
#[must_use]
pub fn graph_proposed_action(repository: &Path, summary: String) -> ProposedAction {
    ProposedAction::CodeGraphQuery {
        repository: repository.display().to_string(),
        summary,
    }
}

/// Parse a `graph.callers_of` / `graph.blast_radius` call: `symbol` is required
/// and non-blank, `depth` optional (clamped downstream, never rejected here — a
/// model that asks for depth 50 gets the ceiling, not a failed call).
pub fn parse_symbol_question(args: &Value, tool: &str) -> Result<(String, usize), String> {
    let symbol = args
        .get("symbol")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{tool} requires a non-empty string `symbol`"))?
        .to_string();
    Ok((symbol, parse_depth(args)))
}

/// Parse a `graph.tests_covering` call: `path` is required and non-blank.
pub fn parse_tests_covering(args: &Value) -> Result<(String, usize), String> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("graph.tests_covering requires a non-empty string `path`")?
        .to_string();
    Ok((path, parse_depth(args)))
}

fn parse_depth(args: &Value) -> usize {
    args.get("depth")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_DEPTH, |depth| {
            usize::try_from(depth).unwrap_or(DEFAULT_DEPTH)
        })
}

/// A one-line summary of a question, for the recorded [`ProposedAction`].
#[must_use]
pub fn summarize_graph_question(question: &GraphQuestion) -> String {
    match question {
        GraphQuestion::Traverse(request) => request.summary(),
        GraphQuestion::CallersOf { symbol } => format!("callers of {symbol}"),
        GraphQuestion::BlastRadius { symbol, depth } => {
            format!("blast radius of {symbol} (depth {depth})")
        }
        GraphQuestion::TestsCovering { path, depth } => {
            format!("tests covering {path} (depth {depth})")
        }
    }
}

/// A row bound is not a text bound when paths or qualified names are very long.
/// Keep the model observation small even if a graph backend returns oversized metadata.
#[must_use]
pub fn render_graph_answer(answer: &codypendent_knowledge::GraphAnswer) -> String {
    const MAX_CHARS: usize = 16_000;
    let rendered = answer.render();
    let Some((end, _)) = rendered.char_indices().nth(MAX_CHARS) else {
        return rendered;
    };
    let prefix = rendered[..end]
        .rsplit_once('\n')
        .map_or(&rendered[..end], |(head, _)| head);
    let mut output = format!(
        "{prefix}\n[Graph output truncated. Narrow the symbol/path or request a smaller limit.]"
    );
    if let Some(page) = &answer.traversal {
        if let Some(next) = page.next_offset {
            output.push_str(&format!("\nnext_offset: {next}"));
        }
    }
    output
}

// --------------------------------------------------------------------------
// `graph.assert_edge` — the agent lever
// --------------------------------------------------------------------------

/// `graph.assert_edge` — record relations the parser cannot see.
///
/// # Why there is no `graph.retract_edge` (yet)
///
/// The obvious companion is a tool that withdraws an assertion, and it is
/// deliberately absent. A delete is the only shape here that can destroy a fact,
/// and the scoping it needs is exactly the scoping that has already been got
/// wrong once in this store — a reparse's edge delete was written without an
/// `evidence_kind` predicate and silently took every semantic edge out of the
/// file with it. A model-callable delete whose predicate slips the same way
/// erases compiler-resolved edges on request.
///
/// It also has less to fix than it looks. A wrong ENDPOINT writes nothing at all
/// (it does not resolve, and the model is told so). A re-assertion is idempotent.
/// A wrong RELATION writes a distinct triple that is labelled `agent_asserted`,
/// carries the run and the reason that produced it, and is outranked by anything
/// mechanical that later contradicts it. So the failure mode is a visible,
/// attributable, low-confidence row — not a corrupted graph.
///
/// When it is worth adding, the shape is: delete restricted to
/// `evidence_kind = 'agent_asserted'` **and** to the session that made the claim,
/// so a run can withdraw its own words and nobody else's.
pub struct GraphAssertEdge;

impl GraphAssertEdge {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "graph.assert_edge";
}

/// How many edges one call may assert. A single turn's worth of reading yields a
/// handful of real relations; a hundred means the model is guessing in bulk, and
/// every one of them would be written at a confidence that outranks the parser's.
pub const MAX_ASSERTED_EDGES: usize = 8;

/// Longest accepted rationale. Long enough for a sentence naming the file and
/// line the model read it off; short enough that provenance stays a label rather
/// than a place to park an essay.
pub const MAX_RATIONALE_CHARS: usize = 400;

/// Longest accepted endpoint name. A symbol name is bounded by the parser, so
/// anything past this is not a name the graph could ever hold.
pub const MAX_SYMBOL_CHARS: usize = 200;

/// The relations an agent may assert, and the wire spelling of each.
///
/// The excluded ones are deliberate, and the line is: **the parser is
/// authoritative for the structure of the graph.** `contains` and `defines` are
/// the containment spine the repository map walks; `imports`, `returns` and
/// `accepts` are read straight off a syntax tree that cannot be wrong about
/// them. An agent adding to those says nothing the fold did not already say, and
/// an agent getting one wrong corrupts a structure other queries trust. What is
/// left is exactly the set whose truth lives *between* files — dispatch,
/// configuration, coverage, dependency — which is the whole reason this tool
/// exists.
pub const ASSERTABLE_RELATIONS: &[(&str, CodeRelation)] = &[
    ("calls", CodeRelation::Calls),
    ("references", CodeRelation::References),
    ("reads", CodeRelation::Reads),
    ("writes", CodeRelation::Writes),
    ("mutates", CodeRelation::Mutates),
    ("configures", CodeRelation::Configures),
    ("tests", CodeRelation::Tests),
    ("implements", CodeRelation::Implements),
    ("extends", CodeRelation::Extends),
    ("serializes", CodeRelation::Serializes),
    ("depends_on", CodeRelation::DependsOn),
    ("generated_from", CodeRelation::GeneratedFrom),
];

/// One edge the model asserts, as parsed off the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertedEdge {
    /// The source symbol, named the way the model saw it in the source
    /// (`decide`, `Router::decide`) — resolved by the same three-tier lookup
    /// `graph.callers_of` uses, never by an internal id.
    pub from: String,
    /// The target symbol, named the same way.
    pub to: String,
    /// What the source does to the target.
    pub relation: CodeRelation,
    /// Why the agent believes this edge holds — REQUIRED, and stored as the
    /// edge's provenance. Two things depend on it. A user reading the graph must
    /// be able to see why a claim is there, since unlike a parsed edge there is
    /// no line of source to point at. And a model made to write the reason down
    /// asserts fewer things it cannot justify — the field is a rubber duck as
    /// much as it is a record.
    pub rationale: String,
}

/// What the daemon seam is asked to write.
#[derive(Debug, Clone, Copy)]
pub struct EdgeAssertionRequest<'a> {
    /// The run's DURABLE repository identity
    /// ([`RunContext::repository_identity`](crate::agent::RunContext::repository_identity)),
    /// from which the implementation derives the SAME `RepositoryId` the scan
    /// folded the graph under — never the worktree the run executes in, which is
    /// a different id and is deleted when the run ends, so a row written under it
    /// is unreachable from every later run and every client, forever.
    /// Server-derived from the run context; never model-supplied.
    pub repository: &'a Path,
    /// The session the assertion was made in — half of the audit trail.
    pub session_id: SessionId,
    /// The run that made it — the other half.
    pub run_id: RunId,
    /// The edges to assert, in the order the model gave them.
    pub edges: &'a [AssertedEdge],
}

/// What became of one asserted edge. Returned one per input edge, in input
/// order, because the model cannot correct a mistake it is only told the *count*
/// of: "1 of 3 unresolved" leaves it re-sending all three and hoping.
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeAssertionOutcome {
    /// Written; no edge for that triple existed.
    Applied,
    /// Written, replacing a strictly less confident edge for the same triple.
    Superseded {
        /// The evidence layer that was replaced.
        previous: EvidenceKind,
        /// Its confidence, so the model can see the ordering that allowed it.
        previous_confidence: f32,
    },
    /// NOT written: an edge for that triple already stands at greater or equal
    /// confidence. Not an error and not worth a retry — the graph already knows.
    Outranked {
        /// The evidence layer that held its ground.
        existing: EvidenceKind,
        /// Its confidence.
        existing_confidence: f32,
    },
    /// NOT written: an endpoint matched no symbol in this repository's graph. An
    /// assertion may not invent a node, so this is a name the model must fix.
    Unresolved {
        /// The endpoint AS THE MODEL WROTE IT, so the observation can quote it.
        symbol: String,
        /// Near names from the same lookup, when there are any — a dead end
        /// turned into a correction.
        candidates: Vec<String>,
    },
    /// NOT written: an endpoint matched several symbols and the tool will not
    /// guess which was meant.
    Ambiguous {
        /// The endpoint as the model wrote it.
        symbol: String,
        /// The matches it has to choose between.
        candidates: Vec<String>,
    },
}

impl EdgeAssertionOutcome {
    /// Whether this outcome put an edge in the graph.
    #[must_use]
    pub fn recorded(&self) -> bool {
        matches!(self, Self::Applied | Self::Superseded { .. })
    }
}

/// The code-graph WRITE surface `graph.assert_edge` depends on — implemented by
/// the daemon assembly over the knowledge pool, exactly as
/// [`RegistrySearch`](super::registry_search::RegistrySearch) is, so this crate
/// needs no `sqlx`.
///
/// Deliberately a separate trait from
/// [`CodeGraphQueries`](codypendent_knowledge::CodeGraphQueries) rather than a
/// method on it: that one is documented, dispositioned by policy, and reasoned
/// about throughout as a pure READ of a derived projection, and quietly giving it
/// a write method would make every one of those statements false. The assembly
/// implements both over the same type, so repository identity is still derived
/// in exactly one place.
#[async_trait]
pub trait CodeGraphAssertions: Send + Sync {
    /// Fold `request.edges` into the repository's graph, returning one outcome
    /// per input edge in input order. An `Err` is a legible message the tool
    /// returns to the model as a failed call.
    async fn assert_edges(
        &self,
        request: EdgeAssertionRequest<'_>,
    ) -> Result<Vec<EdgeAssertionOutcome>, String>;
}

/// The action policy evaluates for a `graph.assert_edge` call: a WRITE to the
/// daemon's own derived code graph — no filesystem, command, network, or remote
/// effect, and no model-supplied path ever reaches the filesystem (an endpoint is
/// matched against stored `code_nodes` rows, and an endpoint that matches nothing
/// is refused rather than created). Allowed without an approval gate for the same
/// reason a `task.*` board write is, and recorded so every assertion is traced
/// and attributable.
///
/// It is NOT [`graph_proposed_action`]: recording a write as a `CodeGraphQuery`
/// would make the audit ledger say the agent read the graph when it changed it.
#[must_use]
pub fn graph_assert_action(repository: &Path, summary: String) -> ProposedAction {
    ProposedAction::CodeGraphAssert {
        repository: repository.display().to_string(),
        summary,
    }
}

/// A one-line summary of an assertion batch, for the recorded [`ProposedAction`].
#[must_use]
pub fn summarize_assertions(edges: &[AssertedEdge]) -> String {
    match edges {
        [single] => format!(
            "assert {} {} {}",
            single.from,
            relation_name(single.relation),
            single.to
        ),
        many => format!("assert {} code-graph edges", many.len()),
    }
}

/// Parse a `graph.assert_edge` call.
///
/// Accepts either a batch (`{"edges": [ … ]}`) or a single flat edge
/// (`{"from": …, "to": …}`), because a model handed a one-element array schema
/// writes the flat form about as often as not, and refusing it costs a turn to
/// teach something that does not matter.
pub fn parse_assert_edge(args: &Value) -> Result<Vec<AssertedEdge>, String> {
    let edges = match args.get("edges") {
        Some(Value::Array(items)) => items.clone(),
        // A model that sends `edges` as a lone object meant a batch of one.
        Some(object @ Value::Object(_)) => vec![object.clone()],
        Some(_) => {
            return Err(format!(
                "{} `edges` must be an array",
                GraphAssertEdge::NAME
            ))
        }
        None => vec![args.clone()],
    };
    if edges.is_empty() {
        return Err(format!(
            "{} needs at least one edge to assert",
            GraphAssertEdge::NAME
        ));
    }
    if edges.len() > MAX_ASSERTED_EDGES {
        return Err(format!(
            "{} accepts at most {MAX_ASSERTED_EDGES} edges per call ({} given) — assert the ones \
             you are sure of, then call again",
            GraphAssertEdge::NAME,
            edges.len()
        ));
    }
    edges.iter().map(parse_one_edge).collect()
}

fn parse_one_edge(edge: &Value) -> Result<AssertedEdge, String> {
    let from = endpoint(edge, "from", &["from_symbol", "source"])?;
    let to = endpoint(edge, "to", &["to_symbol", "target"])?;
    let relation = parse_relation(edge)?;
    let rationale = text(edge, "rationale")
        .or_else(|| text(edge, "reason"))
        .or_else(|| text(edge, "why"))
        .ok_or_else(|| {
            format!(
                "{} requires a `rationale` on every edge — one sentence on how you know this \
                 relation holds. It is stored as the edge's provenance, because unlike a parsed \
                 edge there is no line of source for a reader to check.",
                GraphAssertEdge::NAME
            )
        })?;
    if rationale.chars().count() > MAX_RATIONALE_CHARS {
        return Err(format!(
            "{} `rationale` is longer than {MAX_RATIONALE_CHARS} characters — state how you know, \
             not what you know",
            GraphAssertEdge::NAME
        ));
    }
    Ok(AssertedEdge {
        from,
        to,
        relation,
        rationale,
    })
}

fn endpoint(edge: &Value, key: &str, aliases: &[&str]) -> Result<String, String> {
    let value = aliases
        .iter()
        .fold(text(edge, key), |found, alias| {
            found.or_else(|| text(edge, alias))
        })
        .ok_or_else(|| {
            format!(
                "{} requires a non-empty `{key}` symbol — name it as it appears in the source \
                 (`decide`, `Router::decide`), the same way you would for graph.callers_of",
                GraphAssertEdge::NAME
            )
        })?;
    if value.chars().count() > MAX_SYMBOL_CHARS {
        return Err(format!(
            "{} `{key}` is longer than {MAX_SYMBOL_CHARS} characters — that is not a symbol name",
            GraphAssertEdge::NAME
        ));
    }
    Ok(value)
}

fn parse_relation(edge: &Value) -> Result<CodeRelation, String> {
    let raw = text(edge, "relation")
        .or_else(|| text(edge, "kind"))
        .ok_or_else(|| {
            format!(
                "{} requires a `relation` — one of: {}",
                GraphAssertEdge::NAME,
                assertable_relation_names()
            )
        })?;
    // A model reaches for `Calls`, `CALLS` and `depends-on` as readily as the
    // wire spelling; none of those is a different intent.
    let normalized = raw.to_ascii_lowercase().replace(['-', ' '], "_");
    ASSERTABLE_RELATIONS
        .iter()
        .find(|(name, _)| *name == normalized)
        .map(|(_, relation)| *relation)
        .ok_or_else(|| {
            format!(
                "{} does not assert `{raw}`. Assertable relations are: {}. The others \
                 (`contains`, `defines`, `imports`, `returns`, `accepts`) are what the parser \
                 reads straight off the syntax tree, so the graph already has them.",
                GraphAssertEdge::NAME,
                assertable_relation_names()
            )
        })
}

/// The accepted relation spellings, comma-separated — used in the tool's schema
/// and in every rejection, so the two can never disagree.
#[must_use]
pub fn assertable_relation_names() -> String {
    ASSERTABLE_RELATIONS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The wire spelling of a relation the tool accepted.
#[must_use]
pub fn relation_name(relation: CodeRelation) -> &'static str {
    ASSERTABLE_RELATIONS
        .iter()
        .find(|(_, candidate)| *candidate == relation)
        .map_or("relates_to", |(name, _)| *name)
}

/// A trimmed, non-empty string field, or `None`.
fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|found| !found.is_empty())
        .map(str::to_string)
}

/// The stored `code_edges.evidence_kind` scalar for an evidence layer — the
/// literal column value, so a model told "a compiler_resolved edge already
/// stands here" and a user grepping the table read the same word.
///
/// Derived from the enum's own serialization rather than a hand-written match, so
/// a new evidence layer cannot go missing here.
fn evidence_scalar(kind: EvidenceKind) -> String {
    match serde_json::to_value(kind) {
        Ok(Value::String(text)) => text,
        _ => "unknown".to_string(),
    }
}

/// Render an assertion batch's outcomes as the model-facing observation.
///
/// The contract this exists to keep: **an edge that was not written is named,
/// with the reason and what to do about it.** The engine reports a
/// non-resolving endpoint as a skip, and a skip the model is not shown is a skip
/// the model believes was a success — it moves on with a graph that does not say
/// what it thinks it says.
#[must_use]
pub fn render_edge_assertions(edges: &[AssertedEdge], outcomes: &[EdgeAssertionOutcome]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let recorded = outcomes.iter().filter(|o| o.recorded()).count();
    let _ = writeln!(
        out,
        "graph.assert_edge: {recorded} of {} edge{} recorded on the code graph.",
        edges.len(),
        if edges.len() == 1 { "" } else { "s" }
    );

    for (edge, outcome) in edges.iter().zip(outcomes) {
        let triple = format!(
            "{} --{}--> {}",
            edge.from,
            relation_name(edge.relation),
            edge.to
        );
        let line = match outcome {
            EdgeAssertionOutcome::Applied => format!("  recorded: {triple}"),
            EdgeAssertionOutcome::Superseded {
                previous,
                previous_confidence,
            } => format!(
                "  recorded: {triple} (superseded the {} edge at confidence {previous_confidence:.2})",
                evidence_scalar(*previous)
            ),
            EdgeAssertionOutcome::Outranked {
                existing,
                existing_confidence,
            } => format!(
                "  NOT recorded: {triple} — an existing {} edge already stands here at \
                 confidence {existing_confidence:.2}, which an assertion does not replace. The \
                 graph already knows this; do not retry it.",
                evidence_scalar(*existing)
            ),
            EdgeAssertionOutcome::Unresolved { symbol, candidates } => format!(
                "  NOT recorded: {triple} — `{symbol}` matches no symbol in this repository's \
                 graph, and an assertion cannot create one.{}",
                suggestion(candidates)
            ),
            EdgeAssertionOutcome::Ambiguous { symbol, candidates } => format!(
                "  NOT recorded: {triple} — `{symbol}` matches several symbols and this tool will \
                 not guess which you meant. Name it more fully.{}",
                suggestion(candidates)
            ),
        };
        let _ = writeln!(out, "{line}");
    }

    // Outcomes are one-per-edge in input order; a seam that returns a different
    // count is a bug, but the model must not silently be shown fewer lines than
    // it sent edges.
    if outcomes.len() != edges.len() {
        let _ = writeln!(
            out,
            "  (the graph reported on {} of the {} edges sent)",
            outcomes.len(),
            edges.len()
        );
    }
    out
}

fn suggestion(candidates: &[String]) -> String {
    if candidates.is_empty() {
        " Check the name with graph.callers_of first — the symbol may live in a language or a \
         file the parser has not folded."
            .to_string()
    } else {
        format!(" Did you mean: {}?", candidates.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_requires_a_non_empty_symbol() {
        assert!(parse_symbol_question(&json!({}), GraphCallersOf::NAME).is_err());
        assert!(parse_symbol_question(&json!({"symbol": "  "}), GraphCallersOf::NAME).is_err());
    }

    #[test]
    fn depth_defaults_and_is_carried_through() {
        let (symbol, depth) =
            parse_symbol_question(&json!({"symbol": "Router::decide"}), GraphBlastRadius::NAME)
                .expect("parses");
        assert_eq!(symbol, "Router::decide");
        assert_eq!(depth, DEFAULT_DEPTH);

        let (_, deep) =
            parse_symbol_question(&json!({"symbol": "x", "depth": 4}), GraphBlastRadius::NAME)
                .expect("parses");
        assert_eq!(deep, 4);
    }

    /// An absurd depth is clamped downstream, not refused here: the model asked a
    /// legitimate question badly, and a failed call teaches it nothing.
    #[test]
    fn an_unparseable_depth_falls_back_to_the_default() {
        let (_, depth) = parse_symbol_question(
            &json!({"symbol": "x", "depth": u64::MAX}),
            GraphBlastRadius::NAME,
        )
        .expect("parses");
        assert!(depth >= DEFAULT_DEPTH);
    }

    #[test]
    fn tests_covering_requires_a_path() {
        assert!(parse_tests_covering(&json!({})).is_err());
        let (path, _) = parse_tests_covering(&json!({"path": "src/router.rs"})).expect("parses");
        assert_eq!(path, "src/router.rs");
    }

    /// A model handed a one-element array schema writes the flat object about as
    /// often as not. Both mean the same thing, and refusing one costs a turn.
    #[test]
    fn an_assertion_parses_from_the_batch_and_the_flat_form_alike() {
        let batch = parse_assert_edge(&json!({
            "edges": [{
                "from": "handle_charge",
                "to": "ChargeService::run",
                "relation": "calls",
                "rationale": "routes.rs maps POST /charge to this handler",
            }]
        }))
        .expect("batch form");
        let flat = parse_assert_edge(&json!({
            "from": "handle_charge",
            "to": "ChargeService::run",
            "relation": "calls",
            "rationale": "routes.rs maps POST /charge to this handler",
        }))
        .expect("flat form");
        assert_eq!(batch, flat);
        assert_eq!(batch[0].relation, CodeRelation::Calls);
        assert_eq!(batch[0].from, "handle_charge");
    }

    /// The rationale is what makes an assertion auditable — a parsed edge points
    /// at a line of source, an asserted one has only this.
    #[test]
    fn an_assertion_without_a_rationale_is_refused() {
        let error = parse_assert_edge(&json!({
            "from": "a", "to": "b", "relation": "calls"
        }))
        .unwrap_err();
        assert!(error.contains("rationale"), "{error}");

        // …under any of the names a model reaches for.
        for key in ["rationale", "reason", "why"] {
            let parsed = parse_assert_edge(&json!({
                "from": "a", "to": "b", "relation": "calls", key: "because"
            }))
            .unwrap_or_else(|e| panic!("{key}: {e}"));
            assert_eq!(parsed[0].rationale, "because");
        }
    }

    /// The line: the parser owns the structural relations. An agent adding to
    /// `contains`/`defines` says nothing the fold did not, and an agent getting
    /// one wrong corrupts a spine other queries trust.
    #[test]
    fn only_the_relations_a_parser_cannot_see_are_assertable() {
        for spelling in ["calls", "Calls", "CALLS"] {
            let parsed = parse_assert_edge(&json!({
                "from": "a", "to": "b", "relation": spelling, "rationale": "r"
            }))
            .unwrap_or_else(|e| panic!("{spelling}: {e}"));
            assert_eq!(parsed[0].relation, CodeRelation::Calls);
        }
        // A hyphen is the same intent as an underscore.
        let dashed = parse_assert_edge(&json!({
            "from": "a", "to": "b", "relation": "depends-on", "rationale": "r"
        }))
        .expect("depends-on");
        assert_eq!(dashed[0].relation, CodeRelation::DependsOn);

        for refused in ["contains", "defines", "imports", "returns", "accepts"] {
            let error = parse_assert_edge(&json!({
                "from": "a", "to": "b", "relation": refused, "rationale": "r"
            }))
            .unwrap_err();
            assert!(
                error.contains("parser") && error.contains("calls"),
                "{refused} must be refused with the assertable list: {error}"
            );
        }
    }

    #[test]
    fn an_assertion_batch_is_bounded_in_every_dimension() {
        let one = json!({"from": "a", "to": "b", "relation": "calls", "rationale": "r"});
        let too_many: Vec<Value> = std::iter::repeat_n(one, MAX_ASSERTED_EDGES + 1).collect();
        let error = parse_assert_edge(&json!({ "edges": too_many })).unwrap_err();
        assert!(error.contains("at most"), "{error}");

        let long_rationale = "x".repeat(MAX_RATIONALE_CHARS + 1);
        assert!(parse_assert_edge(&json!({
            "from": "a", "to": "b", "relation": "calls", "rationale": long_rationale
        }))
        .is_err());

        let long_symbol = "x".repeat(MAX_SYMBOL_CHARS + 1);
        assert!(parse_assert_edge(&json!({
            "from": long_symbol, "to": "b", "relation": "calls", "rationale": "r"
        }))
        .is_err());

        assert!(parse_assert_edge(&json!({ "edges": [] })).is_err());
    }

    /// THE reporting contract: an edge that was not written is named, with the
    /// reason and the correction. The engine's answer for an endpoint that
    /// matched nothing is a silent skip, and a skip the model is not shown is a
    /// skip it believes was a success.
    #[test]
    fn every_unwritten_edge_is_named_in_the_observation_with_its_reason() {
        let edges = vec![
            AssertedEdge {
                from: "handle_charge".to_string(),
                to: "ChargeService::run".to_string(),
                relation: CodeRelation::Calls,
                rationale: "the route table".to_string(),
            },
            AssertedEdge {
                from: "handle_refund".to_string(),
                to: "RefundSvc".to_string(),
                relation: CodeRelation::Calls,
                rationale: "guessed".to_string(),
            },
            AssertedEdge {
                from: "decide".to_string(),
                to: "classify".to_string(),
                relation: CodeRelation::Calls,
                rationale: "it looks like it".to_string(),
            },
        ];
        let outcomes = vec![
            EdgeAssertionOutcome::Applied,
            EdgeAssertionOutcome::Unresolved {
                symbol: "RefundSvc".to_string(),
                candidates: vec!["RefundService".to_string()],
            },
            EdgeAssertionOutcome::Outranked {
                existing: EvidenceKind::CompilerResolved,
                existing_confidence: 0.98,
            },
        ];
        let rendered = render_edge_assertions(&edges, &outcomes);

        assert!(rendered.contains("1 of 3 edges recorded"), "{rendered}");
        // The failing endpoint is quoted BY NAME — a count cannot be corrected.
        assert!(rendered.contains("`RefundSvc`"), "{rendered}");
        assert!(rendered.contains("RefundService"), "{rendered}");
        // …and the outranked one is distinguishable from it, because the two
        // need different next moves: fix the name, versus do nothing.
        assert!(rendered.contains("compiler_resolved"), "{rendered}");
        assert!(rendered.contains("do not retry"), "{rendered}");
        assert_eq!(
            rendered.matches("NOT recorded").count(),
            2,
            "both unwritten edges are reported: {rendered}"
        );
    }

    /// A supersession is reported as such: "recorded" alone would hide that a
    /// parsed edge was replaced.
    #[test]
    fn a_supersession_names_what_it_replaced() {
        let edges = vec![AssertedEdge {
            from: "a".to_string(),
            to: "b".to_string(),
            relation: CodeRelation::Configures,
            rationale: "r".to_string(),
        }];
        let rendered = render_edge_assertions(
            &edges,
            &[EdgeAssertionOutcome::Superseded {
                previous: EvidenceKind::SyntaxInferred,
                previous_confidence: 0.45,
            }],
        );
        assert!(rendered.contains("1 of 1 edge recorded"), "{rendered}");
        assert!(rendered.contains("syntax_inferred"), "{rendered}");
        assert!(rendered.contains("0.45"), "{rendered}");
        assert!(rendered.contains("a --configures--> b"), "{rendered}");
    }

    /// A seam that answers with fewer dispositions than it was sent edges is a
    /// bug, but the model must not be quietly shown fewer lines than it sent.
    #[test]
    fn a_short_outcome_list_is_disclosed_rather_than_hidden() {
        let edges = vec![
            AssertedEdge {
                from: "a".to_string(),
                to: "b".to_string(),
                relation: CodeRelation::Calls,
                rationale: "r".to_string(),
            },
            AssertedEdge {
                from: "c".to_string(),
                to: "d".to_string(),
                relation: CodeRelation::Calls,
                rationale: "r".to_string(),
            },
        ];
        let rendered = render_edge_assertions(&edges, &[EdgeAssertionOutcome::Applied]);
        assert!(rendered.contains("reported on 1 of the 2"), "{rendered}");
    }

    #[test]
    fn the_assertion_summary_names_the_triple_or_the_count() {
        let single = vec![AssertedEdge {
            from: "handle_charge".to_string(),
            to: "ChargeService::run".to_string(),
            relation: CodeRelation::Calls,
            rationale: "r".to_string(),
        }];
        assert_eq!(
            summarize_assertions(&single),
            "assert handle_charge calls ChargeService::run"
        );
        let many = vec![single[0].clone(), single[0].clone()];
        assert_eq!(summarize_assertions(&many), "assert 2 code-graph edges");
    }

    #[test]
    fn the_summary_names_the_subject_and_the_depth() {
        assert_eq!(
            summarize_graph_question(&GraphQuestion::CallersOf {
                symbol: "decide".to_string()
            }),
            "callers of decide"
        );
        assert_eq!(
            summarize_graph_question(&GraphQuestion::BlastRadius {
                symbol: "decide".to_string(),
                depth: 3
            }),
            "blast radius of decide (depth 3)"
        );
    }
}
