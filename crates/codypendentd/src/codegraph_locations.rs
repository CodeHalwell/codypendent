use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use codypendent_knowledge::EvidenceRef;
use codypendent_protocol::{
    CodeGraphPage, CodeGraphSourceLocation, CodypendentError, RepositoryId,
};
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Row, SqlitePool};
use tokio::io::AsyncReadExt;

use super::store_error;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PAGE_BYTES: u64 = 32 * 1024 * 1024;

struct ParsedSource {
    text: String,
    sha256: String,
    lines: Vec<usize>,
}

impl ParsedSource {
    fn new(text: String) -> Self {
        let sha256 = hex::encode(Sha256::digest(text.as_bytes()));
        let lines = std::iter::once(0)
            .chain(
                text.bytes()
                    .enumerate()
                    .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
            )
            .collect();
        Self {
            text,
            sha256,
            lines,
        }
    }

    fn position(&self, byte: usize) -> Option<(u32, u32)> {
        if byte > self.text.len() || !self.text.is_char_boundary(byte) {
            return None;
        }
        let line = self
            .lines
            .partition_point(|start| *start <= byte)
            .checked_sub(1)?;
        Some((
            u32::try_from(line + 1).ok()?,
            u32::try_from(self.text[self.lines[line]..byte].chars().count() + 1).ok()?,
        ))
    }
}

struct SourceCache {
    root: Option<PathBuf>,
    sources: HashMap<String, Option<ParsedSource>>,
    remaining: u64,
}

impl SourceCache {
    async fn new(root: &Path) -> Self {
        Self {
            root: tokio::fs::canonicalize(root).await.ok(),
            sources: HashMap::new(),
            remaining: MAX_PAGE_BYTES,
        }
    }

    async fn source(&self, path: &str, length: u64) -> Option<ParsedSource> {
        if length > MAX_FILE_BYTES || length > self.remaining || path.is_empty() {
            return None;
        }
        let relative = Path::new(path);
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return None;
        }
        let root = self.root.as_ref()?;
        let resolved = tokio::fs::canonicalize(root.join(relative)).await.ok()?;
        if !resolved.starts_with(root) {
            return None;
        }
        let file = tokio::fs::File::open(resolved).await.ok()?;
        if file.metadata().await.ok()?.len() != length {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(length + 1).read_to_end(&mut bytes).await.ok()?;
        if bytes.len() as u64 != length {
            return None;
        }
        Some(ParsedSource::new(String::from_utf8(bytes).ok()?))
    }

    async fn location(
        &mut self,
        evidence: &str,
        expected_path: &str,
    ) -> Option<CodeGraphSourceLocation> {
        let EvidenceRef::Artifact {
            artifact,
            source_path: Some(encoded),
        } = serde_json::from_str(evidence).ok()?
        else {
            return None;
        };
        let (path, span) = encoded.rsplit_once('#')?;
        if path != expected_path {
            return None;
        }
        let (start, end) = span.split_once('-')?;
        let start: usize = start.parse().ok()?;
        let end: usize = end.parse().ok()?;
        if start > end {
            return None;
        }
        if !self.sources.contains_key(path) {
            let source = self.source(path, artifact.byte_length).await;
            if let Some(source) = &source {
                self.remaining = self.remaining.saturating_sub(source.text.len() as u64);
            }
            self.sources.insert(path.to_owned(), source);
        }
        let source = self.sources.get(path)?.as_ref()?;
        if source.sha256 != artifact.sha256 || source.text.len() as u64 != artifact.byte_length {
            return None;
        }
        let (line, column) = source.position(start)?;
        let (end_line, end_column) = source.position(end)?;
        Some(CodeGraphSourceLocation {
            source_path: path.to_owned(),
            line,
            column,
            end_line: Some(end_line),
            end_column: Some(end_column),
        })
    }
}

async fn definition_locations(
    pool: &SqlitePool,
    repository: RepositoryId,
    ids: &[String],
    cache: &mut SourceCache,
) -> Result<HashMap<String, CodeGraphSourceLocation>, CodypendentError> {
    let repo = repository.to_string();
    let mut definitions = HashMap::new();
    if !ids.is_empty() {
        let mut builder = QueryBuilder::new(
            "SELECT t.id AS node_id, t.source_path, e.evidence_artifact FROM code_edges e \
             JOIN code_nodes f ON f.id = e.from_node JOIN code_nodes t ON t.id = e.to_node \
             WHERE f.repository = ",
        );
        builder.push_bind(&repo).push(" AND t.repository = ").push_bind(&repo)
            .push(" AND t.kind <> 'external_dependency' AND e.evidence_kind = 'syntax_inferred' AND e.relation IN ('defines', 'contains') AND t.id IN (");
        let mut values = builder.separated(",");
        for id in ids {
            values.push_bind(id);
        }
        values
            .push_unseparated(") ORDER BY CASE e.relation WHEN 'defines' THEN 0 ELSE 1 END, e.id");
        for row in builder.build().fetch_all(pool).await.map_err(store_error)? {
            let id: String = row.get("node_id");
            if definitions.contains_key(&id) {
                continue;
            }
            if let (Some(path), Some(evidence)) = (
                row.get::<Option<String>, _>("source_path"),
                row.get::<Option<String>, _>("evidence_artifact"),
            ) {
                if let Some(location) = cache.location(&evidence, &path).await {
                    definitions.insert(id, location);
                }
            }
        }
    }
    Ok(definitions)
}

pub(crate) async fn for_node_ids(
    pool: &SqlitePool,
    repository: RepositoryId,
    root: &Path,
    ids: &[String],
) -> Result<HashMap<String, CodeGraphSourceLocation>, CodypendentError> {
    let mut cache = SourceCache::new(root).await;
    definition_locations(pool, repository, ids, &mut cache).await
}

/// Coordinates are derived only from the same bytes the parser recorded.
pub(super) async fn attach(
    pool: &SqlitePool,
    repository: RepositoryId,
    root: &Path,
    page: &mut CodeGraphPage,
) -> Result<(), CodypendentError> {
    let mut cache = SourceCache::new(root).await;
    let repo = repository.to_string();
    let ids: Vec<_> = page
        .nodes
        .iter()
        .chain(&page.related_nodes)
        .filter(|node| node.source_path.is_some() && node.kind != "external_dependency")
        .map(|node| node.id.clone())
        .collect();
    let mut definitions = definition_locations(pool, repository, &ids, &mut cache).await?;
    for node in page.nodes.iter_mut().chain(&mut page.related_nodes) {
        node.location = definitions.remove(&node.id);
    }

    let calls: Vec<_> = page
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .collect();
    if calls.is_empty() {
        return Ok(());
    }
    let mut builder = QueryBuilder::new(
        "SELECT e.from_node, e.to_node, e.revision, f.source_path, e.evidence_artifact \
         FROM code_edges e JOIN code_nodes f ON f.id = e.from_node JOIN code_nodes t ON t.id = e.to_node \
         WHERE e.relation = 'calls' AND f.repository = ",
    );
    builder
        .push_bind(&repo)
        .push(" AND t.repository = ")
        .push_bind(&repo)
        .push(" AND (e.from_node, e.to_node, e.revision) IN (");
    for (index, edge) in calls.into_iter().enumerate() {
        if index > 0 {
            builder.push(",");
        }
        builder
            .push("(")
            .push_bind(&edge.from_id)
            .push(",")
            .push_bind(&edge.to_id)
            .push(",")
            .push_bind(&edge.revision)
            .push(")");
    }
    builder.push(")");
    let mut locations = HashMap::new();
    for row in builder.build().fetch_all(pool).await.map_err(store_error)? {
        if let (Some(path), Some(evidence)) = (
            row.get::<Option<String>, _>("source_path"),
            row.get::<Option<String>, _>("evidence_artifact"),
        ) {
            if let Some(location) = cache.location(&evidence, &path).await {
                locations.insert(
                    (
                        row.get::<String, _>("from_node"),
                        row.get::<String, _>("to_node"),
                        row.get::<String, _>("revision"),
                    ),
                    location,
                );
            }
        }
    }
    for edge in page
        .edges
        .iter_mut()
        .filter(|edge| edge.relation == "calls")
    {
        edge.call_site = locations.remove(&(
            edge.from_id.clone(),
            edge.to_id.clone(),
            edge.revision.clone(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{ArtifactId, ArtifactRef, DataClassification};

    fn evidence(path: &str, source: &str, start: usize, end: usize) -> String {
        serde_json::to_string(&EvidenceRef::Artifact {
            artifact: ArtifactRef {
                id: ArtifactId::new(),
                media_type: "text/plain".into(),
                byte_length: source.len() as u64,
                sha256: hex::encode(Sha256::digest(source.as_bytes())),
                sensitivity: DataClassification::Internal,
            },
            source_path: Some(format!("{path}#{start}-{end}")),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn source_locations_are_unicode_aware_and_require_the_exact_recorded_bytes() {
        let repo = tempfile::tempdir().unwrap();
        let source = "// é\r\nfn a() { let _ = \"é\"; call(); }\r\n";
        tokio::fs::write(repo.path().join("main.rs"), source)
            .await
            .unwrap();
        let start = source.find("call()").unwrap();
        let proof = evidence("main.rs", source, start, start + 6);
        let mut cache = SourceCache::new(repo.path()).await;
        let location = cache.location(&proof, "main.rs").await.unwrap();
        assert_eq!(location.line, 2);
        assert_eq!(location.column, 23);
        assert_eq!(location.end_column, Some(29));
        tokio::fs::write(repo.path().join("main.rs"), source.replace("call", "else"))
            .await
            .unwrap();
        assert!(SourceCache::new(repo.path())
            .await
            .location(&proof, "main.rs")
            .await
            .is_none());
        assert!(cache.location(&proof, "different.rs").await.is_none());
        assert!(cache
            .location(&evidence("main.rs", source, 4, 5), "main.rs")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn source_locations_refuse_paths_outside_the_repository() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        tokio::fs::write(outside.path().join("outside.rs"), "call()")
            .await
            .unwrap();
        let mut cache = SourceCache::new(repo.path()).await;
        assert!(cache
            .location(&evidence("../outside.rs", "call()", 0, 6), "../outside.rs")
            .await
            .is_none());
        let absolute = outside.path().join("outside.rs").display().to_string();
        assert!(cache
            .location(&evidence(&absolute, "call()", 0, 6), &absolute)
            .await
            .is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.path().join("outside.rs"),
                repo.path().join("link.rs"),
            )
            .unwrap();
            assert!(cache
                .location(&evidence("link.rs", "call()", 0, 6), "link.rs")
                .await
                .is_none());
        }
    }
}
