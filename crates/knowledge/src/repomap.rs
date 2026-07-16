//! Repository map v1 (Chapter 07 "Repository map", STEP 2.5).
//!
//! Folds the persisted code graph into a compact, revision-aware tree — packages
//! → important modules → public APIs → tests → current change surface — that the
//! agent loop later consumes as a context provider (replacing the Phase 1
//! placeholder). It is a plain data struct with a [`RepositoryMap::render`] that
//! prints the tree; it reads the graph through [`crate::codegraph`] and never
//! parses source itself.
//!
//! Note (v1 deviation): the fixed `code_nodes` schema has no visibility column,
//! so "public APIs" is approximated by the durable API-kind symbols (types,
//! traits, functions, methods, constants). A true `pub` filter needs a schema
//! column and is deferred to the semantic layer (Phase 4). Tests are separated
//! out by their `Test` node kind, and the change surface is an empty stub until
//! revision-diffing lands.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use codypendent_protocol::RepositoryId;
use sqlx::SqlitePool;

use crate::codegraph::{self, last_segment, module_of, CodeGraphError};
use crate::types::CodeNodeKind;

/// A compact, foldable view of a repository's code graph.
#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryMap {
    /// The repository this map describes.
    pub repository: RepositoryId,
    /// The packages (v1: a single synthetic `crate` package, since the syntax
    /// layer does not yet attribute nodes to Cargo packages).
    pub packages: Vec<PackageEntry>,
    /// The "current change surface" — the symbols touched by the active change.
    /// Left empty in v1 (revision-diffing is a later step); kept as a field so
    /// the render and downstream context provider already have the slot.
    pub change_surface: Vec<String>,
}

/// A package and the modules folded under it.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageEntry {
    pub name: String,
    pub modules: Vec<ModuleEntry>,
}

/// A module, its public API surface, and its tests.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleEntry {
    /// The module's qualified path (empty string ⇒ crate root).
    pub name: String,
    pub public_apis: Vec<ApiSymbol>,
    pub tests: Vec<String>,
}

/// One API symbol surfaced in the map.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiSymbol {
    pub name: String,
    pub kind: CodeNodeKind,
}

/// The single synthetic package name used in v1.
const CRATE_PACKAGE: &str = "crate";

/// Build the repository map for `repository` by folding its persisted graph.
pub async fn repository_map(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<RepositoryMap, CodeGraphError> {
    let all = codegraph::nodes(pool, repository).await?;

    // Group by module path. A BTreeMap keeps modules in a stable, sorted order.
    let mut modules: BTreeMap<String, ModuleEntry> = BTreeMap::new();
    let module_entry = |modules: &mut BTreeMap<String, ModuleEntry>, key: &str| {
        modules
            .entry(key.to_owned())
            .or_insert_with(|| ModuleEntry {
                name: key.to_owned(),
                public_apis: Vec::new(),
                tests: Vec::new(),
            });
    };

    for node in &all {
        let qualified = &node.key.qualified_name;
        let simple = last_segment(qualified).to_owned();
        match node.key.kind {
            // A module heads its own group so empty modules still appear.
            CodeNodeKind::Module => module_entry(&mut modules, qualified),
            CodeNodeKind::Test => {
                let key = module_of(qualified);
                module_entry(&mut modules, key);
                modules.get_mut(key).unwrap().tests.push(simple);
            }
            CodeNodeKind::Type
            | CodeNodeKind::TraitOrInterface
            | CodeNodeKind::Function
            | CodeNodeKind::Method
            | CodeNodeKind::Constant => {
                let key = module_of(qualified);
                module_entry(&mut modules, key);
                modules.get_mut(key).unwrap().public_apis.push(ApiSymbol {
                    name: simple,
                    kind: node.key.kind,
                });
            }
            // File and the synthesized reference kinds are not part of the map.
            _ => {}
        }
    }

    // Deterministic ordering within each module.
    let mut modules: Vec<ModuleEntry> = modules.into_values().collect();
    for module in &mut modules {
        module.public_apis.sort_by(|a, b| a.name.cmp(&b.name));
        module.tests.sort();
    }

    let packages = if modules.is_empty() {
        Vec::new()
    } else {
        vec![PackageEntry {
            name: CRATE_PACKAGE.to_owned(),
            modules,
        }]
    };

    Ok(RepositoryMap {
        repository,
        packages,
        change_surface: Vec::new(),
    })
}

impl RepositoryMap {
    /// Render the map as a compact text tree (the agent-context representation).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "repository {}", self.repository);
        for package in &self.packages {
            let _ = writeln!(out, "package {}", package.name);
            for module in &package.modules {
                let label = if module.name.is_empty() {
                    "(crate root)"
                } else {
                    &module.name
                };
                let _ = writeln!(out, "  module {label}");
                for api in &module.public_apis {
                    let _ = writeln!(out, "    {} {}", kind_label(api.kind), api.name);
                }
                for test in &module.tests {
                    let _ = writeln!(out, "    test {test}");
                }
            }
        }
        let surface = if self.change_surface.is_empty() {
            "(none)".to_owned()
        } else {
            self.change_surface.join(", ")
        };
        let _ = writeln!(out, "change surface: {surface}");
        out
    }
}

/// A short label for an API symbol's kind used by [`RepositoryMap::render`].
fn kind_label(kind: CodeNodeKind) -> &'static str {
    match kind {
        CodeNodeKind::Type => "type",
        CodeNodeKind::TraitOrInterface => "trait",
        CodeNodeKind::Function => "fn",
        CodeNodeKind::Method => "method",
        CodeNodeKind::Constant => "const",
        _ => "symbol",
    }
}
