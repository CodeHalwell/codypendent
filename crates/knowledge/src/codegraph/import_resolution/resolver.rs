use super::*;

pub(in crate::codegraph) async fn resolve(
    connection: &mut SqliteConnection,
    repository: RepositoryId,
    path: &str,
    source: &str,
) -> Result<Resolution, CodeGraphError> {
    let Some(language) = language_for(Path::new(path)) else {
        return Ok(Resolution::default());
    };
    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|error| CodeGraphError::Parse(error.to_string()))?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(Resolution::default());
    };
    let root = tree.root_node();
    let mut bindings = match language {
        Language::Python => python_bindings(root, source),
        Language::TypeScript | Language::Tsx | Language::JavaScript => ecma_bindings(root, source),
        Language::Rust => {
            let mut bindings = Vec::new();
            for item in root
                .named_children(&mut root.walk())
                .filter(|node| node.kind() == "use_declaration")
            {
                if let Some(argument) = item.child_by_field_name("argument") {
                    rust_use(argument, source, "", &mut bindings);
                }
            }
            bindings
        }
    };
    let shadowed = shadowed_names(root, source);
    let separator = language.separator();
    let mut counts = HashMap::new();
    for binding in &bindings {
        *counts.entry(binding.local.clone()).or_insert(0) += 1;
    }
    bindings.retain(|binding| {
        counts.get(&binding.local) == Some(&1)
            && !shadowed.contains(binding.local.split(separator).next().unwrap_or(""))
    });
    let mut requested: HashMap<(usize, usize), Vec<(String, String)>> = HashMap::new();
    let mut modules: HashMap<(usize, usize), Vec<HashSet<String>>> = HashMap::new();
    let mut imported = HashMap::new();
    let mut semantics = HashMap::new();
    for (index, binding) in bindings.iter().enumerate() {
        let site = (usize::MAX - index, 0);
        let written = if language == Language::Rust {
            binding.module.clone()
        } else if let Some(symbol) = &binding.symbol {
            let leaf = if language == Language::Python {
                symbol
            } else {
                &binding.local
            };
            join_qualified(&binding.module, language, leaf)
        } else {
            binding.module.clone()
        };
        imported.insert(site, written);
        if language == Language::Rust {
            let mut candidates = rust_targets(path, &binding.module);
            candidates.extend(
                rust_targets(path, &format!("{}::__cody_module__", binding.module))
                    .into_iter()
                    .filter_map(|(path, symbol)| {
                        (symbol == "__cody_module__").then_some((path, String::new()))
                    }),
            );
            requested.insert(site, candidates);
        } else {
            let paths = module_paths(language, path, &binding.module);
            modules
                .entry(site)
                .or_default()
                .push(paths.iter().cloned().collect());
            requested.insert(
                site,
                paths
                    .into_iter()
                    .map(|path| (path, binding.symbol.clone().unwrap_or_default()))
                    .collect(),
            );
            if language == Language::Python {
                if let Some(symbol) = &binding.symbol {
                    let paths = module_paths(
                        language,
                        path,
                        &join_qualified(&binding.module, language, symbol),
                    );
                    modules
                        .entry(site)
                        .or_default()
                        .push(paths.iter().cloned().collect());
                    requested
                        .entry(site)
                        .or_default()
                        .extend(paths.into_iter().map(|path| (path, String::new())));
                }
            }
        }
    }
    let mut pending = vec![(root, 0, false)];
    while let Some((node, depth, nested_module)) = pending.pop() {
        if depth >= MAX_PARSE_DEPTH {
            continue;
        }
        let nested_module =
            nested_module || language == Language::Rust && node.kind() == "mod_item";
        let mut expressions = Vec::new();
        if !nested_module && matches!(node.kind(), "call" | "call_expression") {
            if let Some(function) = node.child_by_field_name("function") {
                expressions.push(((node.start_byte(), node.end_byte()), function));
            }
        }
        if !nested_module {
            let mut types = Vec::new();
            if matches!(
                node.kind(),
                "class_definition" | "class_declaration" | "interface_declaration"
            ) {
                if let Some(owner) = declaration_name(node, source, language) {
                    let heritage = node.child_by_field_name("superclasses").or_else(|| {
                        node.named_children(&mut node.walk()).find(|child| {
                            child.kind() == "class_heritage"
                                || child.kind() == "extends_type_clause"
                        })
                    });
                    if let Some(heritage) = heritage {
                        let clauses: Vec<_> = if heritage.kind() == "class_heritage" {
                            heritage.named_children(&mut heritage.walk()).collect()
                        } else {
                            vec![heritage]
                        };
                        for clause in clauses {
                            let relation = if clause.kind() == "implements_clause" {
                                CodeRelation::Implements
                            } else {
                                CodeRelation::Extends
                            };
                            heritage_types(clause, &mut types, 0);
                            for base in types.drain(..) {
                                let site = (base.start_byte(), base.end_byte());
                                semantics.insert(site, (owner.clone(), relation));
                                expressions.push((site, base));
                            }
                        }
                    }
                }
            }
            if matches!(
                node.kind(),
                "function_definition"
                    | "function_declaration"
                    | "function_item"
                    | "method_definition"
            ) {
                if let Some(owner) = declaration_name(node, source, language) {
                    for field in ["parameters", "return_type"] {
                        if let Some(annotation) = node.child_by_field_name(field) {
                            named_types(annotation, &mut types, 0);
                        }
                    }
                    for annotation in types.drain(..) {
                        let site = (annotation.start_byte(), annotation.end_byte());
                        semantics.insert(site, (owner.clone(), CodeRelation::References));
                        expressions.push((site, annotation));
                    }
                }
            }
            if language == Language::Rust && node.kind() == "impl_item" {
                if let (Some(owner), Some(trait_type)) = (
                    node.child_by_field_name("type"),
                    node.child_by_field_name("trait"),
                ) {
                    let site = (trait_type.start_byte(), trait_type.end_byte());
                    semantics.insert(
                        site,
                        (text(owner, source).to_owned(), CodeRelation::Implements),
                    );
                    expressions.push((site, trait_type));
                }
            }
        }
        for (key, function) in expressions {
            let written = text(function, source);
            if valid_name(written, separator) {
                for binding in &bindings {
                    let tail = if written == binding.local {
                        ""
                    } else if let Some(tail) =
                        written.strip_prefix(&format!("{}{separator}", binding.local))
                    {
                        tail
                    } else {
                        continue;
                    };
                    if language == Language::Rust {
                        let full = if tail.is_empty() {
                            binding.module.clone()
                        } else {
                            format!("{}{separator}{tail}", binding.module)
                        };
                        requested
                            .entry(key)
                            .or_default()
                            .extend(rust_targets(path, &full));
                    } else {
                        let qualified = match (&binding.symbol, tail.is_empty()) {
                            (Some(symbol), true) => symbol.clone(),
                            (Some(symbol), false) => format!("{symbol}{separator}{tail}"),
                            (None, false) => tail.to_owned(),
                            (None, true) => continue,
                        };
                        let paths = module_paths(language, path, &binding.module);
                        modules
                            .entry(key)
                            .or_default()
                            .push(paths.iter().cloned().collect());
                        requested
                            .entry(key)
                            .or_default()
                            .extend(paths.into_iter().map(|path| (path, qualified.clone())));
                        if language == Language::Python && !tail.is_empty() {
                            if let Some(symbol) = &binding.symbol {
                                let paths = module_paths(
                                    language,
                                    path,
                                    &join_qualified(&binding.module, language, symbol),
                                );
                                modules
                                    .entry(key)
                                    .or_default()
                                    .push(paths.iter().cloned().collect());
                                requested
                                    .entry(key)
                                    .or_default()
                                    .extend(paths.into_iter().map(|path| (path, tail.to_owned())));
                            }
                        }
                    }
                }
                if language == Language::Rust
                    && (written.starts_with("crate::")
                        || written.starts_with("self::")
                        || written.starts_with("super::"))
                {
                    requested
                        .entry(key)
                        .or_default()
                        .extend(rust_targets(path, written));
                }
            }
        }
        pending.extend(
            node.named_children(&mut node.walk())
                .map(|child| (child, depth + 1, nested_module)),
        );
    }
    let paths: HashSet<&str> = requested
        .values()
        .flatten()
        .map(|(path, _)| path.as_str())
        .collect();
    if paths.is_empty() {
        return Ok(Resolution::default());
    }
    // SQLite has a bounded bind budget; declining a pathological import file is
    // safer than either an unbounded query or guessing a truncated candidate set.
    if paths.len() > 400 {
        return Ok(Resolution::default());
    }
    let mut query = QueryBuilder::<Sqlite>::new(
        "SELECT id, source_path, qualified_name, kind FROM code_nodes WHERE repository = ",
    );
    query
        .push_bind(repository.to_string())
        .push(" AND source_path IN (");
    let mut separated = query.separated(",");
    for path in paths {
        separated.push_bind(path);
    }
    separated.push_unseparated(") AND kind <> 'external_dependency'");
    let rows = query.build().fetch_all(connection).await?;
    let present: HashSet<String> = rows
        .iter()
        .filter(|row| row.get::<String, _>("kind") == "file")
        .map(|row| row.get("source_path"))
        .collect();
    let mut definitions: HashMap<(String, String), Vec<CodeNodeId>> = HashMap::new();
    let mut kinds = HashMap::new();
    for row in &rows {
        let name = if row.get::<String, _>("kind") == "file" {
            String::new()
        } else {
            row.get("qualified_name")
        };
        let id = CodeNodeId::from_str(&row.get::<String, _>("id"))?;
        definitions
            .entry((row.get("source_path"), name))
            .or_default()
            .push(id);
        kinds.insert(id, row.get::<String, _>("kind"));
    }
    let resolved = requested.into_iter().filter_map(|(site, candidates)| {
        if let Some(paths) = modules.get(&site) {
            if paths
                .iter()
                .any(|paths| paths.intersection(&present).count() > 1)
            {
                return None;
            }
        }
        let targets: HashSet<CodeNodeId> = candidates
            .into_iter()
            .filter(|(candidate, _)| candidate != path)
            .flat_map(|candidate| definitions.get(&candidate).into_iter().flatten().copied())
            .collect();
        (targets.len() == 1).then(|| (site, *targets.iter().next().unwrap()))
    });
    let mut result = Resolution::default();
    for (site, target) in resolved {
        if let Some(written) = imported.get(&site) {
            result.imports.insert(written.clone(), target);
        } else if let Some((from, relation)) = semantics.get(&site) {
            result.relations.push(ResolvedRelation {
                from: from.clone(),
                to: target,
                relation: *relation,
                site,
            });
        } else if kinds
            .get(&target)
            .is_some_and(|kind| matches!(kind.as_str(), "function" | "method" | "test" | "type"))
        {
            result.calls.insert(site, target);
        }
    }
    Ok(result)
}
