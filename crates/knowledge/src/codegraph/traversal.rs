//! Bounded, repository-scoped exploration for the existing graph query seam.
use std::collections::VecDeque;

use super::*;

pub const TRAVERSAL_DEFAULT_LIMIT: usize = 10;
pub const TRAVERSAL_MAX_LIMIT: usize = GRAPH_ANSWER_LIMIT;
pub const TRAVERSAL_MAX_OFFSET: usize = 1_000;
const NODE_BUDGET: usize = 512;
const EXPANSION_BUDGET: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphTraversalKind {
    FindSymbols,
    Definition,
    Callers,
    Callees,
    Neighbors,
    Dependencies,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphDirection {
    Incoming,
    Outgoing,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphTraversal {
    pub kind: GraphTraversalKind,
    pub selector: String,
    pub path: Option<String>,
    pub direction: GraphDirection,
    pub relation: Option<CodeRelation>,
    pub depth: usize,
    pub limit: usize,
    pub offset: usize,
}

impl GraphTraversal {
    #[must_use]
    pub fn summary(&self) -> String {
        let kind = match self.kind {
            GraphTraversalKind::FindSymbols => "find symbols",
            GraphTraversalKind::Definition => "definition",
            GraphTraversalKind::Callers => "callers",
            GraphTraversalKind::Callees => "callees",
            GraphTraversalKind::Neighbors => "neighbors",
            GraphTraversalKind::Dependencies => "dependencies",
        };
        format!(
            "{kind} of `{}`{}",
            self.selector,
            self.path
                .as_ref()
                .map_or_else(String::new, |path| format!(" in {path}"))
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphTraversalMetadata {
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub total_is_exact: bool,
    pub resolved_ids: Vec<CodeNodeId>,
    pub direction: GraphDirection,
    pub depth: usize,
}

const COLUMNS: &str = "n.id, n.language, n.package, n.source_path, n.qualified_name, n.kind, n.signature_hash, n.revision, n.created_at";

fn path_clause(query: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>, path: Option<&str>) {
    if let Some(path) = path {
        let path = path.trim_end_matches('/');
        let escaped = escape_like(path);
        query
            .push(" AND (n.source_path = ")
            .push_bind(path.to_owned())
            .push(" OR n.source_path LIKE ")
            .push_bind(format!("{escaped}/%"))
            .push(" ESCAPE '\\')");
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn matching_query(
    prefix: &str,
    repository: RepositoryId,
    request: &GraphTraversal,
    tier: usize,
) -> sqlx::QueryBuilder<'static, sqlx::Sqlite> {
    let mut query = sqlx::QueryBuilder::new(prefix.to_owned());
    query
        .push(" FROM code_nodes n WHERE n.repository = ")
        .push_bind(repository.to_string());
    match tier {
        0 => {
            query
                .push(" AND (n.id = ")
                .push_bind(request.selector.clone())
                .push(" OR n.qualified_name = ")
                .push_bind(request.selector.clone())
                .push(")");
        }
        1 => {
            let escaped = escape_like(&request.selector);
            query
                .push(" AND (n.qualified_name LIKE ")
                .push_bind(format!("%::{escaped}"))
                .push(" ESCAPE '\\' OR n.qualified_name LIKE ")
                .push_bind(format!("%.{escaped}"))
                .push(" ESCAPE '\\')");
        }
        _ => {
            query
                .push(" AND n.qualified_name LIKE ")
                .push_bind(format!("%{}%", escape_like(&request.selector)))
                .push(" ESCAPE '\\'");
        }
    }
    path_clause(&mut query, request.path.as_deref());
    if request.kind == GraphTraversalKind::Definition {
        query
            .push(" AND n.kind != ")
            .push_bind(scalar(&CodeNodeKind::ExternalDependency));
    }
    query
}

async fn matching_nodes(
    pool: &SqlitePool,
    repository: RepositoryId,
    request: &GraphTraversal,
    limit: usize,
    offset: usize,
) -> Result<(Vec<CodeNode>, usize), CodeGraphError> {
    for tier in 0..3 {
        let total: i64 = matching_query("SELECT COUNT(*)", repository, request, tier)
            .build_query_scalar()
            .fetch_one(pool)
            .await?;
        if total == 0 {
            continue;
        }
        let mut query = matching_query(&format!("SELECT {COLUMNS}"), repository, request, tier);
        query
            .push(" ORDER BY n.qualified_name, n.source_path, n.id LIMIT ")
            .push_bind(limit as i64)
            .push(" OFFSET ")
            .push_bind(offset as i64);
        let rows: Vec<NodeRow> = query.build_query_as().fetch_all(pool).await?;
        return Ok((
            rows.into_iter()
                .map(|row| row.into_node(repository))
                .collect::<Result<_, _>>()?,
            total as usize,
        ));
    }
    Ok((Vec::new(), 0))
}

async fn adjacent(
    pool: &SqlitePool,
    repository: RepositoryId,
    id: CodeNodeId,
    request: &GraphTraversal,
    limit: usize,
) -> Result<Vec<CodeNode>, CodeGraphError> {
    let mut query = sqlx::QueryBuilder::new(format!("SELECT DISTINCT {COLUMNS} FROM code_edges e JOIN code_nodes origin ON origin.id = e.from_node JOIN code_nodes n ON n.id = "));
    match request.direction {
        GraphDirection::Incoming => {
            query.push("e.from_node");
        }
        GraphDirection::Outgoing => {
            query.push("e.to_node");
        }
        GraphDirection::Both => {
            query
                .push("CASE WHEN e.from_node = ")
                .push_bind(id.to_string())
                .push(" THEN e.to_node ELSE e.from_node END");
        }
    }
    query
        .push(" WHERE origin.repository = ")
        .push_bind(repository.to_string())
        .push(" AND n.repository = ")
        .push_bind(repository.to_string());
    match request.direction {
        GraphDirection::Incoming => {
            query.push(" AND e.to_node = ").push_bind(id.to_string());
        }
        GraphDirection::Outgoing => {
            query.push(" AND e.from_node = ").push_bind(id.to_string());
        }
        GraphDirection::Both => {
            query
                .push(" AND (e.from_node = ")
                .push_bind(id.to_string())
                .push(" OR e.to_node = ")
                .push_bind(id.to_string())
                .push(")");
        }
    }
    if let Some(relation) = request.relation {
        query
            .push(" AND e.relation = ")
            .push_bind(scalar(&relation));
    } else if request.kind == GraphTraversalKind::Dependencies {
        query
            .push(" AND e.relation IN (")
            .push_bind(scalar(&CodeRelation::Imports))
            .push(", ")
            .push_bind(scalar(&CodeRelation::DependsOn))
            .push(")");
    }
    query
        .push(" ORDER BY n.qualified_name, n.source_path, n.id LIMIT ")
        .push_bind(limit as i64);
    let rows: Vec<NodeRow> = query.build_query_as().fetch_all(pool).await?;
    rows.into_iter()
        .map(|row| row.into_node(repository))
        .collect()
}

pub(super) async fn answer(
    pool: &SqlitePool,
    repository: RepositoryId,
    input: &GraphTraversal,
) -> Result<GraphAnswer, CodeGraphError> {
    let mut request = input.clone();
    request.selector = request.selector.trim().to_owned();
    if request.selector.is_empty()
        || request.selector.chars().count() > 200
        || request.selector.chars().any(char::is_control)
    {
        return Err(CodeGraphError::Parse(
            "graph traversal requires a bounded symbol name or node ID".into(),
        ));
    }
    if let Some(path) = &request.path {
        if path.len() > 1024
            || Path::new(path).is_absolute()
            || path.contains('\\')
            || path.split('/').any(|part| part == "..")
            || path.chars().any(char::is_control)
        {
            return Err(CodeGraphError::Parse(
                "graph path must be a repository-relative file or directory".into(),
            ));
        }
    }
    request.limit = request.limit.clamp(1, TRAVERSAL_MAX_LIMIT);
    request.offset = request.offset.min(TRAVERSAL_MAX_OFFSET);
    request.depth = request.depth.clamp(1, GRAPH_MAX_DEPTH);
    match request.kind {
        GraphTraversalKind::Callers => {
            request.direction = GraphDirection::Incoming;
            request.relation = Some(CodeRelation::Calls);
            request.depth = 1;
        }
        GraphTraversalKind::Callees => {
            request.direction = GraphDirection::Outgoing;
            request.relation = Some(CodeRelation::Calls);
            request.depth = 1;
        }
        _ => {}
    }
    let lookup = matches!(
        request.kind,
        GraphTraversalKind::FindSymbols | GraphTraversalKind::Definition
    );
    let (seeds, seed_total) = matching_nodes(
        pool,
        repository,
        &request,
        if lookup {
            request.limit
        } else {
            GRAPH_CANDIDATE_LIMIT
        },
        if lookup { request.offset } else { 0 },
    )
    .await?;
    let targets: Vec<String> = seeds
        .iter()
        .map(|node| node.key.qualified_name.clone())
        .collect();
    let resolved_ids: Vec<CodeNodeId> = seeds.iter().map(|node| node.id).collect();
    let (nodes, total, exact) = if lookup {
        (seeds, seed_total, true)
    } else {
        let mut seen: HashSet<CodeNodeId> = resolved_ids.iter().copied().collect();
        let mut queue: VecDeque<_> = resolved_ids.iter().map(|id| (*id, 0_usize)).collect();
        let mut reached = Vec::new();
        let mut exact = seed_total <= GRAPH_CANDIDATE_LIMIT;
        let mut expansions = 0;
        while let Some((id, depth)) = queue.pop_front() {
            if depth >= request.depth {
                continue;
            }
            if expansions >= EXPANSION_BUDGET {
                exact = false;
                break;
            }
            expansions += 1;
            let remaining = NODE_BUDGET.saturating_sub(reached.len());
            let mut neighbors = adjacent(pool, repository, id, &request, remaining + 1).await?;
            if neighbors.len() > remaining {
                neighbors.truncate(remaining);
                exact = false;
            }
            for neighbor in neighbors {
                if seen.insert(neighbor.id) {
                    queue.push_back((neighbor.id, depth + 1));
                    reached.push(neighbor);
                }
            }
            if reached.len() >= NODE_BUDGET {
                exact = false;
                break;
            }
        }
        reached.sort_by(|a, b| {
            a.key
                .qualified_name
                .cmp(&b.key.qualified_name)
                .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
        });
        let total = reached.len();
        (
            reached
                .into_iter()
                .skip(request.offset)
                .take(request.limit)
                .collect(),
            total,
            exact,
        )
    };
    let next_offset = (!nodes.is_empty()
        && request.offset + nodes.len() < total
        && request.offset + nodes.len() <= TRAVERSAL_MAX_OFFSET)
        .then_some(request.offset + nodes.len());
    let mut answer = super::assemble(request.summary(), targets, Vec::new(), nodes);
    answer.total = total;
    answer.traversal = Some(GraphTraversalMetadata {
        offset: request.offset,
        limit: request.limit,
        next_offset,
        total_is_exact: exact,
        resolved_ids,
        direction: request.direction,
        depth: if lookup { 0 } else { request.depth },
    });
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: GraphTraversalKind, selector: &str) -> GraphTraversal {
        GraphTraversal {
            kind,
            selector: selector.into(),
            path: None,
            direction: GraphDirection::Outgoing,
            relation: None,
            depth: 1,
            limit: 10,
            offset: 0,
        }
    }

    async fn fixture() -> (tempfile::TempDir, SqlitePool, RepositoryId) {
        let temp = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&temp.path().join("graph.db"))
            .await
            .unwrap();
        let repository = RepositoryId::new();
        upsert_file_graph(&pool, repository, &GitRevision("r1".into()), "src/lib.rs",
            "pub fn target_alpha() {} pub fn target_beta() { target_alpha(); } pub fn target_gamma() { target_beta(); }").await.unwrap();
        (temp, pool, repository)
    }

    #[tokio::test]
    async fn traversal_pages_names_and_follows_exact_ids_without_enumerating_the_repository() {
        let (_temp, pool, repository) = fixture().await;
        let mut query = request(GraphTraversalKind::FindSymbols, "target_");
        query.limit = 1;
        let first = answer(&pool, repository, &query).await.unwrap();
        assert_eq!(first.total, 3);
        assert_eq!(first.hits.len(), 1);
        assert_eq!(first.traversal.as_ref().unwrap().next_offset, Some(1));
        query.offset = 1;
        let next = answer(&pool, repository, &query).await.unwrap();
        assert_ne!(first.hits[0].id, next.hits[0].id);
        let definition = answer(
            &pool,
            repository,
            &request(
                GraphTraversalKind::Definition,
                &first.hits[0].id.to_string(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(definition.hits[0].id, first.hits[0].id);
        assert_eq!(definition.hits[0].source_path, "src/lib.rs");
        assert!(
            definition.hits[0].location.is_none(),
            "knowledge queries do not invent source coordinates"
        );
        let callers = answer(
            &pool,
            repository,
            &request(
                GraphTraversalKind::Callers,
                &definition.hits[0].id.to_string(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(callers.hits.len(), 1);
        assert!(callers.hits[0].qualified_name.ends_with("target_beta"));
        let callees = answer(
            &pool,
            repository,
            &request(GraphTraversalKind::Callees, &callers.hits[0].id.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(callees.hits[0].id, definition.hits[0].id);
        assert!(callers.render().contains("node_id:"));
    }

    #[tokio::test]
    async fn traversal_filters_relations_paths_and_excludes_foreign_repository_edges() {
        let (_temp, pool, repository) = fixture().await;
        let alpha = find_symbols(&pool, repository, "target_alpha", 1)
            .await
            .unwrap()
            .remove(0);
        let beta = find_symbols(&pool, repository, "target_beta", 1)
            .await
            .unwrap()
            .remove(0);
        let foreign = RepositoryId::new();
        upsert_file_graph(
            &pool,
            foreign,
            &GitRevision("r1".into()),
            "private.rs",
            "fn hidden() {}",
        )
        .await
        .unwrap();
        let hidden = find_symbols(&pool, foreign, "hidden", 1)
            .await
            .unwrap()
            .remove(0);
        for (from, to, relation) in [
            (beta.id, alpha.id, "depends_on"),
            (alpha.id, hidden.id, "calls"),
            (hidden.id, alpha.id, "calls"),
        ] {
            sqlx::query("INSERT INTO code_edges(id,from_node,to_node,relation,confidence,evidence_kind,revision,created_at) VALUES(?,?,?,?,1.0,'syntax_inferred','r1',?)")
                .bind(uuid::Uuid::now_v7().to_string()).bind(from.to_string()).bind(to.to_string()).bind(relation).bind(chrono::Utc::now().to_rfc3339()).execute(&pool).await.unwrap();
        }
        let dependencies = answer(
            &pool,
            repository,
            &request(GraphTraversalKind::Dependencies, &beta.id.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(dependencies.hits.len(), 1);
        assert_eq!(dependencies.hits[0].id, alpha.id);
        let mut neighbors = request(GraphTraversalKind::Neighbors, &alpha.id.to_string());
        neighbors.direction = GraphDirection::Both;
        neighbors.relation = Some(CodeRelation::Calls);
        let result = answer(&pool, repository, &neighbors).await.unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].id, beta.id);
        upsert_file_graph(
            &pool,
            repository,
            &GitRevision("r1".into()),
            "src/consumer.rs",
            "fn elsewhere() {}",
        )
        .await
        .unwrap();
        let elsewhere = find_symbols(&pool, repository, "elsewhere", 1)
            .await
            .unwrap()
            .remove(0);
        sqlx::query("INSERT INTO code_edges(id,from_node,to_node,relation,confidence,evidence_kind,revision,created_at) VALUES(?,?,?,'calls',1.0,'syntax_inferred','r1',?)")
            .bind(uuid::Uuid::now_v7().to_string()).bind(elsewhere.id.to_string()).bind(alpha.id.to_string()).bind(chrono::Utc::now().to_rfc3339()).execute(&pool).await.unwrap();
        let mut scoped = request(GraphTraversalKind::Callers, "target_alpha");
        scoped.path = Some("src/lib.rs".into());
        let cross_file = answer(&pool, repository, &scoped).await.unwrap();
        assert!(cross_file.hits.iter().any(|hit| hit.id == elsewhere.id), "definition path disambiguates the target without hiding callers in other repository files");
        neighbors.path = Some("other".into());
        assert!(answer(&pool, repository, &neighbors)
            .await
            .unwrap()
            .hits
            .is_empty());
        neighbors.path = Some("../outside".into());
        assert!(answer(&pool, repository, &neighbors).await.is_err());
        assert!(answer(
            &pool,
            repository,
            &request(GraphTraversalKind::FindSymbols, "%_")
        )
        .await
        .unwrap()
        .hits
        .is_empty());
    }

    #[tokio::test]
    async fn traversal_hub_output_and_work_are_bounded_with_an_explicit_partial_count() {
        let temp = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&temp.path().join("hub.db")).await.unwrap();
        let repository = RepositoryId::new();
        let source = format!(
            "fn hub() {{}} {}",
            (0..530)
                .map(|index| format!("fn caller_{index:04}() {{ hub(); }}"))
                .collect::<String>()
        );
        upsert_file_graph(
            &pool,
            repository,
            &GitRevision("r1".into()),
            "src/hub.rs",
            &source,
        )
        .await
        .unwrap();
        let mut query = request(GraphTraversalKind::Callers, "hub");
        query.limit = usize::MAX;
        let result = answer(&pool, repository, &query).await.unwrap();
        assert_eq!(result.hits.len(), GRAPH_ANSWER_LIMIT);
        assert!(result.total <= NODE_BUDGET);
        let metadata = result.traversal.as_ref().unwrap();
        assert!(!metadata.total_is_exact);
        assert_eq!(metadata.next_offset, Some(GRAPH_ANSWER_LIMIT));
        assert!(result.render().contains("Traversal budget reached"));
        assert!(result.render().len() < 20_000);
    }
}
