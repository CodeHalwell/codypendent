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

/// Cap on the API names sampled per module before folding the remainder into a
/// `(+K more)` tail. [`RepositoryMap::render`] is a bounded SUMMARY, not a dump:
/// a module with hundreds of symbols still contributes only a handful of lines —
/// the agent has `workspace.read_file`/search tools for the rest (the Chapter 07
/// transcript-declutter fix; a flat per-symbol render once flooded a run's
/// opening context with ~25 KB for a repository of a few thousand symbols).
const MAX_SAMPLED_APIS_PER_MODULE: usize = 8;

/// Cap on the modules rendered per package before folding the remainder into a
/// `(+K more modules)` tail — the safety net that keeps the render bounded for a
/// *large* repository (hundreds of modules), complementing
/// [`MAX_SAMPLED_APIS_PER_MODULE`] which only bounds a single module's symbols.
///
/// Selects the most API-rich modules (by public API count, then test count,
/// then path, for determinism) — NOT simply the first `MAX_RENDERED_MODULES`
/// entries of `package.modules`, which is path-sorted; a naive prefix would
/// show an arbitrary alphabetical slice instead of the modules that actually
/// carry the repository's structure. See [`select_modules`]. The selected
/// modules still render in path order for readability.
const MAX_RENDERED_MODULES: usize = 50;

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
    /// Render the map as a compact, BOUNDED summary — the agent-context
    /// representation (the Chapter 07 transcript-declutter fix). Each module
    /// renders its API/test counts plus a capped sample of API names
    /// ([`MAX_SAMPLED_APIS_PER_MODULE`]), never the full per-symbol dump; a
    /// package with more modules than [`MAX_RENDERED_MODULES`] shows only the
    /// most significant ones ([`select_modules`]), folding the remainder into a
    /// `(+K more modules)` tail.
    ///
    /// This trims the model's actual context, not just a display: the full
    /// symbol list serves a small local model poorly and burns tokens the agent
    /// doesn't need up front — it has search/read tools for anything this
    /// summary doesn't cover.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "repository {}", self.repository);
        for package in &self.packages {
            let _ = writeln!(out, "package {}", package.name);
            let total_modules = package.modules.len();
            let shown = select_modules(&package.modules, MAX_RENDERED_MODULES);
            for module in &shown {
                let label = if module.name.is_empty() {
                    "(crate root)"
                } else {
                    &module.name
                };
                let _ = writeln!(
                    out,
                    "  module {label} — {} APIs, {} tests",
                    module.public_apis.len(),
                    module.tests.len()
                );
                if !module.public_apis.is_empty() {
                    let _ = writeln!(out, "    {}", sample_apis(&module.public_apis));
                }
            }
            let hidden_modules = total_modules - shown.len();
            if hidden_modules > 0 {
                let _ = writeln!(out, "  … (+{hidden_modules} more modules)");
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

// --------------------------------------------------------------------------
// Hierarchical map (STEP 4.5) — workspace → package → module, with evidence
// --------------------------------------------------------------------------

/// The level of a [`MapNode`] in the hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapLevel {
    Workspace,
    Package,
    Module,
    Symbol,
}

/// Why a map node exists — the evidence a hierarchical map records at each level
/// so the TUI can show why a symbol entered context (Chapter 07). `revision` is
/// the graph revision that produced the node; `symbol_count` is how many durable
/// symbols are folded beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEvidence {
    pub revision: Option<String>,
    pub symbol_count: usize,
}

/// One node of the hierarchical repository map. Built **bottom-up**: symbol leaves
/// aggregate into modules, modules into packages, packages into the workspace,
/// each parent's evidence summing its children's.
#[derive(Debug, Clone, PartialEq)]
pub struct MapNode {
    pub label: String,
    pub level: MapLevel,
    pub evidence: MapEvidence,
    pub children: Vec<MapNode>,
}

/// Build the hierarchical repository map (workspace → package → module → symbol),
/// bottom-up, each node recording the evidence (revision + symbol count) that
/// produced it. For very large repositories this is the compact, foldable form
/// the context builder surfaces instead of one flat symbol list.
pub async fn hierarchical_map(
    pool: &SqlitePool,
    repository: RepositoryId,
) -> Result<MapNode, CodeGraphError> {
    let all = codegraph::nodes(pool, repository).await?;

    // module path -> (symbol leaves, revisions seen)
    let mut modules: BTreeMap<String, Vec<MapNode>> = BTreeMap::new();
    for node in &all {
        let leaf_kind = matches!(
            node.key.kind,
            CodeNodeKind::Type
                | CodeNodeKind::TraitOrInterface
                | CodeNodeKind::Function
                | CodeNodeKind::Method
                | CodeNodeKind::Constant
                | CodeNodeKind::Test
        );
        if !leaf_kind {
            continue;
        }
        let module = module_of(&node.key.qualified_name).to_owned();
        modules.entry(module).or_default().push(MapNode {
            label: last_segment(&node.key.qualified_name).to_owned(),
            level: MapLevel::Symbol,
            evidence: MapEvidence {
                revision: Some(node.revision.0.clone()),
                symbol_count: 1,
            },
            children: Vec::new(),
        });
    }

    // Fold symbols into module nodes (bottom-up: the module's evidence is the sum
    // of its symbols').
    let mut module_nodes = Vec::new();
    for (module, mut symbols) in modules {
        symbols.sort_by(|a, b| a.label.cmp(&b.label));
        let revision = symbols.iter().find_map(|s| s.evidence.revision.clone());
        let symbol_count = symbols.len();
        module_nodes.push(MapNode {
            label: if module.is_empty() {
                "(crate root)".to_owned()
            } else {
                module
            },
            level: MapLevel::Module,
            evidence: MapEvidence {
                revision,
                symbol_count,
            },
            children: symbols,
        });
    }

    // Fold modules into the single synthetic package, and the package into the
    // workspace root. (The syntax layer does not yet attribute nodes to Cargo
    // packages; the semantic adapter's `build_metadata` supplies real packages.)
    let total: usize = module_nodes.iter().map(|m| m.evidence.symbol_count).sum();
    let revision = module_nodes
        .iter()
        .find_map(|m| m.evidence.revision.clone());
    let package = MapNode {
        label: CRATE_PACKAGE.to_owned(),
        level: MapLevel::Package,
        evidence: MapEvidence {
            revision: revision.clone(),
            symbol_count: total,
        },
        children: module_nodes,
    };
    Ok(MapNode {
        label: repository.to_string(),
        level: MapLevel::Workspace,
        evidence: MapEvidence {
            revision,
            symbol_count: total,
        },
        children: if package.evidence.symbol_count == 0 && package.children.is_empty() {
            Vec::new()
        } else {
            vec![package]
        },
    })
}

impl MapNode {
    /// Render the hierarchy as an indented tree, annotating each node with the
    /// evidence (symbol count + revision) that produced it.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(0, &mut out);
        out
    }

    fn render_into(&self, depth: usize, out: &mut String) {
        let indent = "  ".repeat(depth);
        let level = match self.level {
            MapLevel::Workspace => "workspace",
            MapLevel::Package => "package",
            MapLevel::Module => "module",
            MapLevel::Symbol => "symbol",
        };
        let rev = self.evidence.revision.as_deref().unwrap_or("-");
        let _ = writeln!(
            out,
            "{indent}{level} {} [{} symbols @ {rev}]",
            self.label, self.evidence.symbol_count
        );
        for child in &self.children {
            child.render_into(depth + 1, out);
        }
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

/// Render a module's API sample for [`RepositoryMap::render`]: up to
/// [`MAX_SAMPLED_APIS_PER_MODULE`] entries as `kind name`, comma-joined, with a
/// trailing `(+K more)` once the module holds more than the cap.
fn sample_apis(apis: &[ApiSymbol]) -> String {
    let shown = apis.len().min(MAX_SAMPLED_APIS_PER_MODULE);
    let mut sample: Vec<String> = apis[..shown]
        .iter()
        .map(|api| format!("{} {}", kind_label(api.kind), api.name))
        .collect();
    let hidden = apis.len() - shown;
    if hidden > 0 {
        sample.push(format!("… (+{hidden} more)"));
    }
    sample.join(", ")
}

/// Select up to `cap` of `modules` to render for [`RepositoryMap::render`].
///
/// When `modules.len() <= cap`, every module is kept, in its original (path)
/// order — no ranking needed. Otherwise the SELECTION is by significance —
/// most public APIs first, ties broken by most tests, then by path for
/// determinism — so a large repository's `(+K more modules)` cap hides the
/// least-informative modules rather than an arbitrary alphabetical tail
/// (`modules` is path-sorted, so a naive `[..cap]` prefix would show only
/// whichever modules happen to sort early). The selected set is then
/// re-ordered by path before returning, so the RENDER stays readable
/// top-to-bottom by module path even though selection was by significance.
fn select_modules(modules: &[ModuleEntry], cap: usize) -> Vec<&ModuleEntry> {
    if modules.len() <= cap {
        return modules.iter().collect();
    }
    let mut ranked: Vec<&ModuleEntry> = modules.iter().collect();
    ranked.sort_by(|a, b| {
        b.public_apis
            .len()
            .cmp(&a.public_apis.len())
            .then_with(|| b.tests.len().cmp(&a.tests.len()))
            .then_with(|| a.name.cmp(&b.name))
    });
    ranked.truncate(cap);
    ranked.sort_by(|a, b| a.name.cmp(&b.name));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under both caps, `render` shows every API by name and the exact counts —
    /// the same information the old per-symbol dump carried for a module this
    /// small, just with the counts made explicit.
    #[test]
    fn render_shows_full_sample_and_counts_under_the_caps() {
        let map = RepositoryMap {
            repository: RepositoryId::new(),
            packages: vec![PackageEntry {
                name: CRATE_PACKAGE.to_owned(),
                modules: vec![ModuleEntry {
                    name: String::new(),
                    public_apis: vec![
                        ApiSymbol {
                            name: "Engine".to_owned(),
                            kind: CodeNodeKind::Type,
                        },
                        ApiSymbol {
                            name: "compute".to_owned(),
                            kind: CodeNodeKind::Function,
                        },
                    ],
                    tests: vec!["engine_ticks".to_owned()],
                }],
            }],
            change_surface: Vec::new(),
        };
        let rendered = map.render();
        assert!(
            rendered.contains("module (crate root) — 2 APIs, 1 tests"),
            "counts line:\n{rendered}"
        );
        assert!(
            rendered.contains("type Engine, fn compute"),
            "full sample under the cap:\n{rendered}"
        );
        assert!(
            !rendered.contains("more)"),
            "nothing hidden under either cap:\n{rendered}"
        );
        assert!(rendered.contains("change surface: (none)"));
    }

    /// A module with more APIs than [`MAX_SAMPLED_APIS_PER_MODULE`] caps its
    /// sample and folds the remainder into an exact `(+K more)` tail — the count
    /// line still reports the true total, never the capped sample size.
    #[test]
    fn render_caps_the_api_sample_with_a_more_tail() {
        let total = MAX_SAMPLED_APIS_PER_MODULE + 3;
        let public_apis: Vec<ApiSymbol> = (0..total)
            .map(|i| ApiSymbol {
                name: format!("sym{i:02}"),
                kind: CodeNodeKind::Function,
            })
            .collect();
        let map = RepositoryMap {
            repository: RepositoryId::new(),
            packages: vec![PackageEntry {
                name: CRATE_PACKAGE.to_owned(),
                modules: vec![ModuleEntry {
                    name: "big".to_owned(),
                    public_apis,
                    tests: Vec::new(),
                }],
            }],
            change_surface: Vec::new(),
        };
        let rendered = map.render();
        assert!(
            rendered.contains(&format!("module big — {total} APIs, 0 tests")),
            "count line reports the true total, not the capped sample:\n{rendered}"
        );
        assert!(
            rendered.contains("fn sym00"),
            "first sampled name:\n{rendered}"
        );
        assert!(
            rendered.contains(&format!("fn sym{:02}", MAX_SAMPLED_APIS_PER_MODULE - 1)),
            "the cap-th symbol is still sampled:\n{rendered}"
        );
        assert!(
            rendered.contains("… (+3 more)"),
            "exact hidden count in the tail:\n{rendered}"
        );
        assert!(
            !rendered.contains(&format!("fn sym{:02}", MAX_SAMPLED_APIS_PER_MODULE)),
            "the (cap+1)-th symbol must not be individually named:\n{rendered}"
        );
    }

    /// A package with more modules than [`MAX_RENDERED_MODULES`] caps the
    /// modules shown and folds the remainder into an exact `(+K more modules)`
    /// tail — the safety net that bounds the render for a large repository
    /// (many modules), distinct from the per-module API cap. Every module here
    /// is equally (in)significant (0 APIs, 0 tests), so [`select_modules`]'s
    /// significance ranking is a pure tie, resolved by its path tie-break —
    /// this pins that fallback down to a path-ordered prefix, exactly as
    /// before selection existed.
    #[test]
    fn render_caps_modules_per_package_with_a_more_modules_tail() {
        let total = MAX_RENDERED_MODULES + 4;
        let modules: Vec<ModuleEntry> = (0..total)
            .map(|i| ModuleEntry {
                name: format!("m{i:03}"),
                public_apis: Vec::new(),
                tests: Vec::new(),
            })
            .collect();
        let map = RepositoryMap {
            repository: RepositoryId::new(),
            packages: vec![PackageEntry {
                name: CRATE_PACKAGE.to_owned(),
                modules,
            }],
            change_surface: Vec::new(),
        };
        let rendered = map.render();
        assert!(
            rendered.contains("module m000 —"),
            "first shown:\n{rendered}"
        );
        assert!(
            rendered.contains(&format!("module m{:03} —", MAX_RENDERED_MODULES - 1)),
            "the cap-th module is still shown:\n{rendered}"
        );
        assert!(
            !rendered.contains(&format!("module m{:03} —", MAX_RENDERED_MODULES)),
            "the (cap+1)-th module must not be individually shown:\n{rendered}"
        );
        assert!(
            rendered.contains("(+4 more modules)"),
            "exact hidden module count in the tail:\n{rendered}"
        );
    }

    /// When modules differ in significance, the cap must select the most
    /// API-rich ones — NOT an arbitrary alphabetical prefix of the (path-
    /// sorted) module list. Constructed so the API-rich modules sort dead
    /// LAST alphabetically: under the old "first `MAX_RENDERED_MODULES` in
    /// path order" behavior every one of them would be hidden, so this test
    /// would fail under that code. The selected set still RENDERS in path
    /// order for readability — significance only decides WHICH modules are
    /// kept, not the order they're printed in.
    #[test]
    fn render_selects_the_most_api_rich_modules_not_an_alphabetical_prefix() {
        // Path-first, API-poor (1 API each) — sorts ahead of every rich module.
        let low_count = MAX_RENDERED_MODULES + 2;
        let mut modules: Vec<ModuleEntry> = (0..low_count)
            .map(|i| ModuleEntry {
                name: format!("a_plain_{i:03}"),
                public_apis: vec![ApiSymbol {
                    name: "f".to_owned(),
                    kind: CodeNodeKind::Function,
                }],
                tests: Vec::new(),
            })
            .collect();
        // Path-last, API-rich (10 APIs each) — the modules that actually carry
        // the repository's structure, but alphabetically after every plain one.
        for i in 0..5 {
            modules.push(ModuleEntry {
                name: format!("z_rich_{i:02}"),
                public_apis: (0..10)
                    .map(|j| ApiSymbol {
                        name: format!("api{j:02}"),
                        kind: CodeNodeKind::Function,
                    })
                    .collect(),
                tests: Vec::new(),
            });
        }
        let total_modules = modules.len();
        let map = RepositoryMap {
            repository: RepositoryId::new(),
            packages: vec![PackageEntry {
                name: CRATE_PACKAGE.to_owned(),
                modules,
            }],
            change_surface: Vec::new(),
        };
        let rendered = map.render();

        // Every API-rich module is shown despite sorting dead last — proving
        // selection is by significance, not a path-ordered prefix.
        for i in 0..5 {
            assert!(
                rendered.contains(&format!("module z_rich_{i:02} —")),
                "an API-rich module must be shown regardless of its path:\n{rendered}"
            );
        }
        // A low-significance module yields its slot to make room.
        assert!(
            !rendered.contains(&format!("module a_plain_{:03} —", low_count - 1)),
            "a low-significance module must be squeezed out:\n{rendered}"
        );
        // The hidden count is exact: total modules minus the cap.
        let hidden = total_modules - MAX_RENDERED_MODULES;
        assert!(
            rendered.contains(&format!("(+{hidden} more modules)")),
            "exact hidden module count:\n{rendered}"
        );
        // The selected set still renders in path order: every shown
        // "a_plain_*" line precedes every "z_rich_*" line.
        let first_rich = rendered
            .find("module z_rich_00 —")
            .expect("the first rich module is shown");
        let last_plain = rendered
            .rfind("module a_plain_")
            .expect("plain modules are shown");
        assert!(
            last_plain < first_rich,
            "the selected modules render in path order, not significance order:\n{rendered}"
        );
    }

    /// Fix 2 leaves the change-surface slot untouched: still a joined list, or
    /// the `(none)` stub when empty.
    #[test]
    fn render_preserves_change_surface() {
        let map = RepositoryMap {
            repository: RepositoryId::new(),
            packages: Vec::new(),
            change_surface: vec!["a::b".to_owned(), "c::d".to_owned()],
        };
        assert!(map.render().contains("change surface: a::b, c::d"));
    }
}
