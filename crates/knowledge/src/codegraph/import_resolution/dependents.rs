use super::*;

fn mentions_file(language: Language, source: &str, written: &str, changed: &HashSet<&str>) -> bool {
    if language == Language::Rust {
        return rust_targets(source, written)
            .into_iter()
            .chain(rust_targets(source, &format!("{written}::__cody_module__")))
            .any(|(path, _)| changed.contains(path.as_str()));
    }
    let mut module = written;
    loop {
        if module_paths(language, source, module)
            .iter()
            .any(|path| changed.contains(path.as_str()))
        {
            return true;
        }
        let Some((prefix, _)) = module.rsplit_once('.') else {
            return false;
        };
        if prefix.is_empty() || prefix == module {
            return false;
        }
        module = prefix;
    }
}

pub(in crate::codegraph) async fn affected_importers_page(
    pool: &SqlitePool,
    repository: RepositoryId,
    changed_paths: &[String],
    after: Option<&str>,
    limit: usize,
) -> Result<AffectedImporters, CodeGraphError> {
    let limit = limit.clamp(1, 2000);
    let changed: HashSet<&str> = changed_paths.iter().map(String::as_str).collect();
    if changed.is_empty() {
        return Ok(AffectedImporters::default());
    }
    let mut paths = Vec::new();
    let mut cursor = after.unwrap_or("").to_owned();
    loop {
        // Keyset-page metadata only, without reading or parsing source files.
        let batch: Vec<String> = sqlx::query_scalar("SELECT DISTINCT n.source_path FROM code_nodes n JOIN code_edges e ON e.from_node = n.id WHERE n.repository = ? AND n.source_path > ? AND e.relation = 'imports' ORDER BY n.source_path LIMIT 200")
            .bind(repository.to_string()).bind(&cursor).fetch_all(pool).await?;
        if batch.is_empty() {
            break;
        }
        cursor = batch.last().unwrap().clone();
        let mut query = QueryBuilder::<Sqlite>::new("SELECT f.source_path, t.source_path AS target_path, t.kind, t.qualified_name FROM code_edges e JOIN code_nodes f ON f.id = e.from_node JOIN code_nodes t ON t.id = e.to_node WHERE f.repository = ");
        query
            .push_bind(repository.to_string())
            .push(" AND t.repository = ")
            .push_bind(repository.to_string())
            .push(" AND e.relation = 'imports' AND f.source_path IN (");
        let mut list = query.separated(",");
        for path in &batch {
            list.push_bind(path);
        }
        list.push_unseparated(")");
        let mut affected = HashSet::new();
        for row in query.build().fetch_all(pool).await? {
            let source: String = row.get("source_path");
            if changed.contains(source.as_str()) {
                continue;
            }
            let target: String = row.get("target_path");
            let explicit_target = row.get::<String, _>("kind") != "external_dependency"
                && changed.contains(target.as_str());
            let unresolved_target = row.get::<String, _>("kind") == "external_dependency"
                && language_for(Path::new(&source)).is_some_and(|language| {
                    mentions_file(
                        language,
                        &source,
                        &row.get::<String, _>("qualified_name"),
                        &changed,
                    )
                });
            if explicit_target || unresolved_target {
                affected.insert(source);
            }
        }
        for path in batch {
            if affected.contains(&path) {
                paths.push(path);
            }
        }
        if paths.len() > limit {
            paths.truncate(limit);
            return Ok(AffectedImporters {
                paths,
                truncated: true,
            });
        }
    }
    Ok(AffectedImporters {
        paths,
        truncated: false,
    })
}
