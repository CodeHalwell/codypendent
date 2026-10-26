use super::*;

/// The second pass makes full scans independent of file order. Existing source
/// evidence is retained; only syntax calls with an explicit unique target move.
pub(in crate::codegraph) async fn reconcile_file(
    pool: &SqlitePool,
    repository: RepositoryId,
    path: &str,
    source: &str,
) -> Result<(), CodeGraphError> {
    let mut tx = pool.begin().await?;
    let targets = resolve(&mut tx, repository, path, source).await?;
    if targets.calls.is_empty() && targets.imports.is_empty() && targets.relations.is_empty() {
        return Ok(());
    }
    let rows = sqlx::query("SELECT e.id, e.from_node, e.to_node, t.qualified_name AS written, e.relation, e.confidence, e.evidence_artifact, e.revision FROM code_edges e JOIN code_nodes n ON n.id = e.from_node JOIN code_nodes t ON t.id = e.to_node WHERE n.repository = ? AND n.source_path = ? AND e.relation IN ('calls','imports') AND e.evidence_kind = 'syntax_inferred'")
        .bind(repository.to_string()).bind(path).fetch_all(&mut *tx).await?;
    let source_hash = hex::encode(Sha256::digest(source.as_bytes()));
    for row in rows {
        let Some(raw) = row.get::<Option<String>, _>("evidence_artifact") else {
            continue;
        };
        let evidence: EvidenceRef = serde_json::from_str(&raw)?;
        let EvidenceRef::Artifact {
            artifact,
            source_path: Some(location),
        } = &evidence
        else {
            continue;
        };
        if artifact.sha256 != source_hash {
            continue;
        }
        let Some(span) = location.strip_prefix(&format!("{path}#")) else {
            continue;
        };
        let Some((start, end)) = span
            .split_once('-')
            .and_then(|(start, end)| Some((start.parse().ok()?, end.parse().ok()?)))
        else {
            continue;
        };
        let relation: CodeRelation = from_scalar(&row.get::<String, _>("relation"))?;
        let target = if relation == CodeRelation::Calls {
            targets.calls.get(&(start, end))
        } else {
            targets.imports.get(&row.get::<String, _>("written"))
        };
        let Some(target) = target else {
            continue;
        };
        if row.get::<String, _>("to_node") == target.to_string() {
            continue;
        }
        let from = CodeNodeId::from_str(&row.get::<String, _>("from_node"))?;
        sqlx::query("DELETE FROM code_edges WHERE id = ?")
            .bind(row.get::<String, _>("id"))
            .execute(&mut *tx)
            .await?;
        fold_edge(
            &mut tx,
            &CodeEdge {
                from,
                to: *target,
                relation,
                confidence: row.get::<f64, _>("confidence") as f32,
                evidence_kind: EvidenceKind::SyntaxInferred,
                evidence: Some(evidence),
                revision: GitRevision(row.get("revision")),
            },
            &Utc::now().to_rfc3339(),
        )
        .await?;
        outbox::enqueue(
            &mut *tx,
            &KnowledgeIndexEvent::SymbolChanged(from),
            Utc::now(),
        )
        .await?;
    }
    add_relations(&mut tx, repository, path, source, &targets.relations).await?;
    prune_references(&mut tx, repository, path).await?;
    tx.commit().await?;
    Ok(())
}

pub(in crate::codegraph) async fn prune_references(
    connection: &mut SqliteConnection,
    repository: RepositoryId,
    path: &str,
) -> Result<Vec<CodeNodeId>, CodeGraphError> {
    let unused: Vec<(String,)> = sqlx::query_as("SELECT id FROM code_nodes n WHERE repository = ? AND source_path = ? AND kind = 'external_dependency' AND NOT EXISTS (SELECT 1 FROM code_edges e WHERE e.from_node = n.id OR e.to_node = n.id)")
        .bind(repository.to_string()).bind(path).fetch_all(&mut *connection).await?;
    let mut removed = Vec::new();
    for (id,) in unused {
        sqlx::query("DELETE FROM code_nodes WHERE id = ?")
            .bind(&id)
            .execute(&mut *connection)
            .await?;
        removed.push(CodeNodeId::from_str(&id)?);
    }
    Ok(removed)
}

pub(in crate::codegraph) async fn add_relations(
    connection: &mut SqliteConnection,
    repository: RepositoryId,
    path: &str,
    source: &str,
    relations: &[ResolvedRelation],
) -> Result<Vec<CodeEdge>, CodeGraphError> {
    if relations.is_empty() {
        return Ok(Vec::new());
    }
    let built = build_file_graph(repository, path, source)?;
    let mut result = Vec::new();
    for resolved in relations {
        let owners: Vec<(String, String)> = sqlx::query_as("SELECT id, revision FROM code_nodes WHERE repository = ? AND source_path = ? AND qualified_name = ? AND kind <> 'external_dependency'")
            .bind(repository.to_string()).bind(path).bind(&resolved.from).fetch_all(&mut *connection).await?;
        if owners.len() != 1 {
            continue;
        }
        let record = CodeEdge {
            from: CodeNodeId::from_str(&owners[0].0)?,
            to: resolved.to,
            relation: resolved.relation,
            confidence: SYNTAX_CALL_CONFIDENCE,
            evidence_kind: EvidenceKind::SyntaxInferred,
            evidence: Some(EvidenceRef::Artifact {
                artifact: built.file_artifact.clone(),
                source_path: Some(format!("{path}#{}-{}", resolved.site.0, resolved.site.1)),
            }),
            revision: GitRevision(owners[0].1.clone()),
        };
        if matches!(
            fold_edge(connection, &record, &Utc::now().to_rfc3339()).await?,
            EdgeFold::Written { .. }
        ) {
            outbox::enqueue(
                &mut *connection,
                &KnowledgeIndexEvent::SymbolChanged(record.from),
                Utc::now(),
            )
            .await?;
            result.push(record);
        }
    }
    Ok(result)
}
