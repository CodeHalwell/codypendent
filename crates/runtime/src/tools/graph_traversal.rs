//! Parsers and schemas for the existing repository-scoped graph read family.
use codypendent_knowledge::codegraph::{
    GraphDirection, GraphTraversal, GraphTraversalKind, GRAPH_MAX_DEPTH, TRAVERSAL_DEFAULT_LIMIT,
    TRAVERSAL_MAX_LIMIT, TRAVERSAL_MAX_OFFSET,
};
use codypendent_knowledge::CodeRelation;
use serde_json::{json, Value};

pub const GRAPH_TRAVERSAL_TOOLS: &[(&str, GraphTraversalKind, &str)] = &[
    ("graph.find_symbols", GraphTraversalKind::FindSymbols, "Find repository symbols by name. Returns bounded IDs, kinds, source paths and revisions. Start here, then pass a returned node ID to graph.definition or a traversal tool. Never enumerate the whole graph into context."),
    ("graph.definition", GraphTraversalKind::Definition, "Locate a definition by exact node ID or source symbol name. Returns source location when verified, plus path/kind/revision metadata. Use path to disambiguate a repository-relative file or directory."),
    ("graph.callers_of", GraphTraversalKind::Callers, "Find direct callers of a function by node ID or source name. Returns a bounded page of caller IDs and source metadata. Recorded calls are evidence, not proof of all possible runtime usages."),
    ("graph.callees_of", GraphTraversalKind::Callees, "Find functions directly called by a symbol. Pass a node ID from graph.find_symbols; returns a bounded page of callee IDs and source metadata."),
    ("graph.neighbors", GraphTraversalKind::Neighbors, "Traverse a symbol's recorded relationships with direction, relation, depth and path filters. Results are bounded; follow returned IDs or next_offset instead of loading an entire graph."),
    ("graph.dependencies", GraphTraversalKind::Dependencies, "Traverse recorded imports and depends_on relationships. Supports incoming/outgoing/both direction, bounded depth and optional relation/path filters. Missing relationships are unknown, not proof of independence."),
];

pub fn traversal_kind(tool: &str) -> Option<GraphTraversalKind> {
    GRAPH_TRAVERSAL_TOOLS
        .iter()
        .find(|(name, _, _)| *name == tool)
        .map(|(_, kind, _)| *kind)
}

pub fn parse_graph_traversal(args: &Value, tool: &str) -> Result<GraphTraversal, String> {
    let kind =
        traversal_kind(tool).ok_or_else(|| format!("unknown graph traversal tool {tool}"))?;
    let key = if kind == GraphTraversalKind::FindSymbols {
        "query"
    } else {
        "symbol"
    };
    let selector = args
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{tool} requires a non-empty string `{key}`"))?;
    if selector.chars().count() > 200 || selector.chars().any(char::is_control) {
        return Err(format!(
            "{tool} {key} must contain at most 200 safe characters"
        ));
    }
    let path = match args.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.trim().is_empty() => None,
        Some(Value::String(value)) => {
            let value = value.trim();
            if value.len() > 1024
                || std::path::Path::new(value).is_absolute()
                || value.contains('\\')
                || value.split('/').any(|part| part == "..")
                || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "{tool} path must be a repository-relative file or directory"
                ));
            }
            Some(value.to_owned())
        }
        _ => return Err(format!("{tool} path must be a string")),
    };
    let direction = match args.get("direction") {
        None => GraphDirection::Outgoing,
        Some(Value::String(value)) if value == "incoming" => GraphDirection::Incoming,
        Some(Value::String(value)) if value == "outgoing" => GraphDirection::Outgoing,
        Some(Value::String(value)) if value == "both" => GraphDirection::Both,
        _ => {
            return Err(format!(
                "{tool} direction must be incoming, outgoing or both"
            ))
        }
    };
    let relation = match args.get("relation") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(
            serde_json::from_value::<CodeRelation>(json!(value))
                .map_err(|_| format!("{tool} relation is not a recorded graph relation"))?,
        ),
        _ => return Err(format!("{tool} relation must be a string")),
    };
    Ok(GraphTraversal {
        kind,
        selector: selector.to_owned(),
        path,
        direction,
        relation,
        depth: number(args, "depth", 1, GRAPH_MAX_DEPTH)?,
        limit: number(args, "limit", TRAVERSAL_DEFAULT_LIMIT, TRAVERSAL_MAX_LIMIT)?,
        offset: number(args, "offset", 0, TRAVERSAL_MAX_OFFSET)?,
    })
}

fn number(args: &Value, key: &str, default: usize, max: usize) -> Result<usize, String> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .map(|value| {
                usize::try_from(value)
                    .unwrap_or(max)
                    .min(max)
                    .max(usize::from(key != "offset"))
            })
            .ok_or_else(|| format!("graph {key} must be a non-negative integer")),
    }
}

pub fn graph_traversal_schema(kind: GraphTraversalKind) -> Value {
    let key = if kind == GraphTraversalKind::FindSymbols {
        "query"
    } else {
        "symbol"
    };
    let mut properties = json!({
        "path": {"type":"string", "description":"Optional repository-relative file or directory for resolving the target. Traversal can follow recorded links into other files of the same repository."},
        "limit": {"type":"integer", "minimum":1, "maximum":TRAVERSAL_MAX_LIMIT, "default":TRAVERSAL_DEFAULT_LIMIT},
        "offset": {"type":"integer", "minimum":0, "maximum":TRAVERSAL_MAX_OFFSET, "default":0}
    });
    properties[key] = json!({"type":"string", "minLength":1, "maxLength":200,
        "description":if kind == GraphTraversalKind::FindSymbols { "A specific symbol name or fragment; no wildcard repository dump." } else { "Stable node ID from graph.find_symbols or the source symbol name." }});
    if matches!(
        kind,
        GraphTraversalKind::Neighbors | GraphTraversalKind::Dependencies
    ) {
        properties["depth"] =
            json!({"type":"integer", "minimum":1, "maximum":GRAPH_MAX_DEPTH, "default":1});
        properties["direction"] =
            json!({"type":"string", "enum":["incoming","outgoing","both"], "default":"outgoing"});
        properties["relation"] = json!({"type":"string", "enum":["contains","defines","imports","references","calls","implements","extends","reads","writes","mutates","returns","accepts","tests","configures","serializes","depends_on","generated_from"]});
    }
    json!({"type":"object", "properties":properties, "required":[key]})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_arguments_are_bounded_and_repository_scoped() {
        let request = parse_graph_traversal(&json!({"symbol":"work", "limit":999,"depth":999,"offset":99999,"direction":"incoming","relation":"calls","path":"src"}), "graph.neighbors").unwrap();
        assert_eq!(
            (request.limit, request.depth, request.offset),
            (40, 5, 1000)
        );
        assert_eq!(request.direction, GraphDirection::Incoming);
        assert_eq!(request.relation, Some(CodeRelation::Calls));
        for args in [
            json!({"symbol":"x","path":"../outside"}),
            json!({"symbol":"x","path":"/outside"}),
            json!({"symbol":"x","limit":"all"}),
            json!({"symbol":"x","direction":"sideways"}),
            json!({"symbol":"x","relation":"invented"}),
        ] {
            assert!(parse_graph_traversal(&args, "graph.neighbors").is_err());
        }
        assert!(parse_graph_traversal(&json!({"query":""}), "graph.find_symbols").is_err());
    }

    #[test]
    fn graph_observation_caps_oversized_utf8_metadata_and_retains_pagination() {
        use codypendent_knowledge::codegraph::{GraphAnswer, GraphHit, GraphTraversalMetadata};
        let answer = GraphAnswer {
            question: "bounded metadata".into(),
            targets: Vec::new(),
            candidates: Vec::new(),
            total: 2,
            hits: vec![GraphHit {
                id: codypendent_protocol::CodeNodeId::new(),
                location: None,
                qualified_name: "é".repeat(20_000),
                kind: codypendent_knowledge::CodeNodeKind::Function,
                source_path: "src/file.rs".into(),
                revision: "r1".into(),
            }],
            traversal: Some(GraphTraversalMetadata {
                offset: 0,
                limit: 1,
                next_offset: Some(1),
                total_is_exact: true,
                resolved_ids: Vec::new(),
                direction: GraphDirection::Outgoing,
                depth: 1,
            }),
        };
        let rendered = super::super::graph::render_graph_answer(&answer);
        assert!(rendered.chars().count() < 16_200);
        assert!(rendered.contains("Graph output truncated"));
        assert!(rendered.contains("next_offset: 1"));
    }
}
