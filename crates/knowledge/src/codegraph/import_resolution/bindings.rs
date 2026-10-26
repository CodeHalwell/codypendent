use super::*;

pub(super) fn python_bindings(root: Node, source: &str) -> Vec<Binding> {
    let mut bindings = Vec::new();
    for statement in root.named_children(&mut root.walk()) {
        if statement.kind() == "import_from_statement" {
            let Some(module) = statement.child_by_field_name("module_name") else {
                continue;
            };
            for item in statement.children_by_field_name("name", &mut statement.walk()) {
                let name = item.child_by_field_name("name").unwrap_or(item);
                let original = text(name, source);
                if original == "*" {
                    continue;
                }
                let alias = item
                    .child_by_field_name("alias")
                    .map_or(original, |alias| text(alias, source));
                bindings.push(Binding {
                    local: alias.to_owned(),
                    module: text(module, source).to_owned(),
                    symbol: Some(original.to_owned()),
                });
            }
        } else if statement.kind() == "import_statement" {
            for item in statement.named_children(&mut statement.walk()) {
                if !matches!(item.kind(), "dotted_name" | "aliased_import") {
                    continue;
                }
                let original = text(item.child_by_field_name("name").unwrap_or(item), source);
                let local = item
                    .child_by_field_name("alias")
                    .map_or(original, |alias| text(alias, source));
                bindings.push(Binding {
                    local: local.to_owned(),
                    module: original.to_owned(),
                    symbol: None,
                });
            }
        }
    }
    bindings
}

pub(super) fn ecma_bindings(root: Node, source: &str) -> Vec<Binding> {
    let mut bindings = Vec::new();
    for statement in root.named_children(&mut root.walk()) {
        if statement.kind() != "import_statement"
            || text(statement, source)
                .trim_start()
                .starts_with("import type ")
        {
            continue;
        }
        let Some(module) = statement.child_by_field_name("source") else {
            continue;
        };
        let module = text(module, source).trim_matches(['\'', '"']);
        if !module.starts_with('.') {
            continue;
        }
        let mut pending = vec![statement];
        while let Some(item) = pending.pop() {
            match item.kind() {
                "import_specifier" if !text(item, source).trim_start().starts_with("type ") => {
                    let Some(name) = item.child_by_field_name("name") else {
                        continue;
                    };
                    bindings.push(Binding {
                        local: text(item.child_by_field_name("alias").unwrap_or(name), source)
                            .to_owned(),
                        module: module.to_owned(),
                        symbol: Some(text(name, source).to_owned()),
                    });
                }
                "namespace_import" => {
                    if let Some(name) = item.named_child(0) {
                        bindings.push(Binding {
                            local: text(name, source).to_owned(),
                            module: module.to_owned(),
                            symbol: None,
                        });
                    }
                }
                "import_statement" | "import_clause" | "named_imports" => {
                    pending.extend(item.named_children(&mut item.walk()))
                }
                _ => {}
            }
        }
    }
    bindings
}

pub(super) fn rust_use(node: Node, source: &str, prefix: &str, bindings: &mut Vec<Binding>) {
    let join = |tail: &str| {
        if prefix.is_empty() {
            tail.to_owned()
        } else {
            format!("{prefix}::{tail}")
        }
    };
    match node.kind() {
        "scoped_use_list" => {
            let Some(path) = node.child_by_field_name("path") else {
                return;
            };
            let prefix = join(text(path, source));
            if let Some(list) = node.child_by_field_name("list") {
                for child in list.named_children(&mut list.walk()) {
                    rust_use(child, source, &prefix, bindings);
                }
            }
        }
        "use_list" => {
            for child in node.named_children(&mut node.walk()) {
                rust_use(child, source, prefix, bindings);
            }
        }
        "use_as_clause" => {
            let (Some(path), Some(alias)) = (
                node.child_by_field_name("path"),
                node.child_by_field_name("alias"),
            ) else {
                return;
            };
            bindings.push(Binding {
                local: text(alias, source).to_owned(),
                module: join(text(path, source)),
                symbol: None,
            });
        }
        _ => {
            let path = join(text(node, source));
            if let Some(local) = path
                .rsplit("::")
                .next()
                .filter(|local| *local != "*" && *local != "self")
            {
                bindings.push(Binding {
                    local: local.to_owned(),
                    module: path,
                    symbol: None,
                });
            }
        }
    }
}
