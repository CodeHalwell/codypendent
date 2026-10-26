use super::*;

pub(super) fn declaration_name(node: Node, source: &str, language: Language) -> Option<String> {
    let name = text(node.child_by_field_name("name")?, source);
    let mut parts = vec![name];
    let mut parent = node.parent();
    while let Some(node) = parent {
        if matches!(
            node.kind(),
            "class_definition" | "class_declaration" | "mod_item" | "internal_module"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                parts.push(text(name, source));
            }
        }
        parent = node.parent();
    }
    parts.reverse();
    Some(parts.join(language.separator()))
}

pub(super) fn named_types<'a>(
    node: Node<'a>,
    source: &str,
    output: &mut Vec<Node<'a>>,
    depth: usize,
) {
    if depth >= MAX_PARSE_DEPTH {
        return;
    }
    if matches!(
        node.kind(),
        "identifier" | "type_identifier" | "attribute" | "scoped_type_identifier"
    ) {
        output.push(node);
        return;
    }
    for child in node.named_children(&mut node.walk()) {
        named_types(child, source, output, depth + 1);
    }
}

pub(super) fn heritage_types<'a>(node: Node<'a>, output: &mut Vec<Node<'a>>, depth: usize) {
    if depth >= MAX_PARSE_DEPTH {
        return;
    }
    match node.kind() {
        "identifier" | "type_identifier" | "attribute" | "scoped_type_identifier" => {
            output.push(node)
        }
        "generic_type" | "subscript" => {
            if let Some(base) = node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("value"))
            {
                heritage_types(base, output, depth + 1);
            }
        }
        "argument_list" | "extends_clause" | "extends_type_clause" | "implements_clause" => {
            for child in node.named_children(&mut node.walk()) {
                heritage_types(child, output, depth + 1);
            }
        }
        _ => {}
    }
}

pub(super) fn text<'a>(node: Node, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

pub(super) fn identifiers(node: Node, source: &str, names: &mut HashSet<String>) {
    let mut pending = vec![(node, 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth >= MAX_PARSE_DEPTH {
            continue;
        }
        if matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) {
            names.insert(text(node, source).to_owned());
        }
        pending.extend(
            node.named_children(&mut node.walk())
                .map(|child| (child, depth + 1)),
        );
    }
}

pub(super) fn shadowed_names(root: Node, source: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut pending = vec![(root, 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth >= MAX_PARSE_DEPTH {
            continue;
        }
        let field = match node.kind() {
            "assignment"
            | "augmented_assignment"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "for_statement" => Some("left"),
            "variable_declarator"
            | "named_expression"
            | "function_definition"
            | "function_declaration"
            | "class_definition"
            | "class_declaration"
            | "function_item" => Some("name"),
            "let_declaration" => Some("pattern"),
            _ => None,
        };
        if let Some(target) = field.and_then(|field| node.child_by_field_name(field)) {
            identifiers(target, source, &mut names);
        }
        if matches!(node.kind(), "parameters" | "formal_parameters") {
            for parameter in node.named_children(&mut node.walk()) {
                let binding = if parameter.kind() == "identifier" {
                    Some(parameter)
                } else {
                    parameter
                        .child_by_field_name("pattern")
                        .or_else(|| parameter.child_by_field_name("name"))
                        .or_else(|| parameter.named_child(0))
                };
                if let Some(binding) = binding {
                    identifiers(binding, source, &mut names);
                }
            }
        }
        if node.kind() == "delete_statement" {
            identifiers(node, source, &mut names);
        }
        pending.extend(
            node.named_children(&mut node.walk())
                .map(|child| (child, depth + 1)),
        );
    }
    names
}

pub(super) fn valid_name(value: &str, separator: &str) -> bool {
    !value.is_empty()
        && value.split(separator).all(|part| {
            let mut chars = part.chars();
            chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_')
        })
}
