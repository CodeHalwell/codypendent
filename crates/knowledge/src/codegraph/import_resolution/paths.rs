use super::*;

pub(super) fn normalized(path: &str) -> Option<String> {
    if path.starts_with('/') || path.contains(['\\', '\0', ':']) {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

pub(super) fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

pub(super) fn module_paths(language: Language, path: &str, module: &str) -> Vec<String> {
    match language {
        Language::Python => {
            let relative = module.chars().take_while(|c| *c == '.').count();
            let module = &module[relative..];
            if !module.is_empty() && !valid_name(module, ".") {
                return Vec::new();
            }
            let bases = if relative > 0 {
                let mut base = parent(path);
                for _ in 1..relative {
                    let Some((next, _)) = base.rsplit_once('/') else {
                        return Vec::new();
                    };
                    base = next;
                }
                vec![format!("{base}/{}", module.replace('.', "/"))]
            } else {
                vec![
                    module.replace('.', "/"),
                    format!("src/{}", module.replace('.', "/")),
                ]
            };
            bases
                .into_iter()
                .filter_map(|base| normalized(&base))
                .flat_map(|base| [format!("{base}.py"), format!("{base}/__init__.py")])
                .collect()
        }
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            if !module.starts_with('.') {
                return Vec::new();
            }
            let Some(base) = normalized(&format!("{}/{module}", parent(path))) else {
                return Vec::new();
            };
            let extensions = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
            if extensions
                .iter()
                .any(|extension| base.ends_with(&format!(".{extension}")))
            {
                // NodeNext source imports may name the emitted .js file.
                let mut paths = vec![base.clone()];
                if let Some(stem) = base.strip_suffix(".js") {
                    paths.extend([format!("{stem}.ts"), format!("{stem}.tsx")]);
                }
                paths
            } else {
                extensions
                    .iter()
                    .flat_map(|extension| {
                        [
                            format!("{base}.{extension}"),
                            format!("{base}/index.{extension}"),
                        ]
                    })
                    .collect()
            }
        }
        Language::Rust => Vec::new(),
    }
}

/// Candidate (file, qualified-name) pairs for an explicit in-crate Rust path.
pub(super) fn rust_targets(path: &str, written: &str) -> Vec<(String, String)> {
    let root = if path.starts_with("src/") {
        "src"
    } else if let Some((prefix, _)) = path.rsplit_once("/src/") {
        return rust_targets_at(&format!("{prefix}/src"), path, written);
    } else {
        return Vec::new();
    };
    rust_targets_at(root, path, written)
}

pub(super) fn rust_targets_at(root: &str, path: &str, written: &str) -> Vec<(String, String)> {
    if !valid_name(written, "::") || path.starts_with(&format!("{root}/bin/")) {
        return Vec::new();
    }
    let suffix = path.strip_prefix(&format!("{root}/")).unwrap_or("");
    let mut current: Vec<&str> = suffix.trim_end_matches(".rs").split('/').collect();
    if matches!(current.last(), Some(&"mod" | &"lib" | &"main")) {
        current.pop();
    }
    let mut segments: Vec<&str> = written.split("::").collect();
    match segments.first().copied() {
        Some("crate") => {
            current.clear();
            segments.remove(0);
        }
        Some("self") => {
            segments.remove(0);
        }
        Some("super") => {
            while segments.first() == Some(&"super") {
                if current.pop().is_none() {
                    return Vec::new();
                }
                segments.remove(0);
            }
        }
        _ => return Vec::new(),
    }
    current.extend(segments);
    let mut targets = Vec::new();
    for split in 1..current.len() {
        let module = current[..split].join("/");
        let symbol = current[split..].join("::");
        targets.extend([
            (format!("{root}/{module}.rs"), symbol.clone()),
            (format!("{root}/{module}/mod.rs"), symbol),
        ]);
    }
    // Inline modules are unambiguous only if exactly one crate root matches.
    let symbol = current.join("::");
    targets.extend([
        (format!("{root}/lib.rs"), symbol.clone()),
        (format!("{root}/main.rs"), symbol),
    ]);
    targets
}
