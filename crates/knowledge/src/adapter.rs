//! Language adapters (Chapter 07, STEP 4.5).
//!
//! A [`LanguageAdapter`] presents a uniform surface over a language's tooling:
//! `parse` (syntax symbols), `symbols` (a workspace index), `diagnostics`, and
//! `build_metadata`. Each adapter reports its best available
//! [`SemanticCapability`]: if a language server (rust-analyzer, pyright,
//! typescript-language-server) is found on `PATH` it can resolve references at
//! LSP confidence; otherwise it **degrades gracefully to the syntax layer** at
//! the lower syntax confidence — never failing, just less precise.
//!
//! Rust is the first-class adapter (its syntax layer is the Phase 2 tree-sitter
//! graph, its `build_metadata` is `cargo metadata`, its `diagnostics` are
//! `cargo check --message-format=json`). Python and TypeScript are deliberately
//! thinner: a line-level syntax scan with optional LSP when present.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::codegraph::{self, ParsedSymbol};
use crate::types::LanguageId;

/// A workspace an adapter operates over (its filesystem root).
#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// A single file to parse.
#[derive(Debug, Clone)]
pub struct ParseInput {
    /// Repo-relative path (used to derive module qualification).
    pub path: String,
    pub source: String,
}

/// The result of a syntax parse: the durable symbols the file defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOutput {
    pub language: LanguageId,
    pub symbols: Vec<ParsedSymbol>,
}

/// A workspace-wide symbol index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolIndex {
    pub language: LanguageId,
    /// `(repo-relative path, symbols in that file)`.
    pub files: Vec<(String, Vec<ParsedSymbol>)>,
}

impl SymbolIndex {
    /// Total symbol count across all files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.iter().map(|(_, s)| s.len()).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A compiler/linter diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: String,
    pub line: u32,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

/// Build/package metadata for a workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildMetadata {
    pub packages: Vec<PackageInfo>,
    pub dependencies: Vec<String>,
}

/// One package in the build metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
}

/// The best semantic tier an adapter can produce in the current environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticCapability {
    /// No language server found — syntax layer only (lower confidence).
    SyntaxOnly,
    /// A language server is available; references can be resolved at LSP
    /// confidence.
    LspResolved,
}

/// Errors from an adapter.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("parse error: {0}")]
    Parse(#[from] codegraph::CodeGraphError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool `{tool}` failed: {reason}")]
    Tool { tool: String, reason: String },
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// A language's tooling surface (Chapter 07 `LanguageAdapter`).
#[async_trait]
pub trait LanguageAdapter: Send + Sync {
    /// The language this adapter serves.
    fn language(&self) -> LanguageId;

    /// The best semantic tier available now (LSP if its server is on `PATH`, else
    /// syntax-only).
    fn capability(&self) -> SemanticCapability;

    /// Parse one file into the symbols it defines (syntax layer).
    async fn parse(&self, input: ParseInput) -> Result<ParseOutput, AdapterError>;

    /// Index every source file in the workspace.
    async fn symbols(&self, workspace: &Workspace) -> Result<SymbolIndex, AdapterError>;

    /// Compiler/linter diagnostics for the workspace. Degrades to an empty list
    /// when no compiler is available rather than failing.
    async fn diagnostics(&self, workspace: &Workspace) -> Result<Vec<Diagnostic>, AdapterError>;

    /// Build/package metadata for the workspace.
    async fn build_metadata(&self, workspace: &Workspace) -> Result<BuildMetadata, AdapterError>;
}

/// Whether `bin` is an executable on `PATH` — the graceful-degradation probe.
#[must_use]
pub fn on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file() || candidate.with_extension("exe").is_file()
    })
}

/// Recursively collect files under `root` whose extension is in `exts`.
fn collect_sources(root: &Path, exts: &[&str]) -> Vec<PathBuf> {
    fn walk(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let path = entry.path();
            // Use the entry's own file type, which does NOT follow the final
            // symlink — a circular directory symlink would otherwise recurse
            // forever and overflow the stack when scanning an untrusted workspace.
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir {
                // Skip the usual noise directories.
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(
                    name,
                    "target" | "node_modules" | ".git" | "dist" | "__pycache__"
                ) {
                    continue;
                }
                walk(&path, exts, out);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if exts.contains(&ext) {
                    out.push(path);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, exts, &mut out);
    out
}

/// The repo-relative path of `file` under `root` (falling back to the file name).
fn rel_path(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

// --------------------------------------------------------------------------
// Rust adapter (first-class)
// --------------------------------------------------------------------------

/// The Rust adapter: tree-sitter syntax (Phase 2 graph), `cargo metadata`, and
/// `cargo check` diagnostics; rust-analyzer resolution when it is on `PATH`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RustAdapter;

#[async_trait]
impl LanguageAdapter for RustAdapter {
    fn language(&self) -> LanguageId {
        LanguageId("rust".into())
    }

    fn capability(&self) -> SemanticCapability {
        if on_path("rust-analyzer") {
            SemanticCapability::LspResolved
        } else {
            SemanticCapability::SyntaxOnly
        }
    }

    async fn parse(&self, input: ParseInput) -> Result<ParseOutput, AdapterError> {
        let symbols = codegraph::parse_symbols(&input.path, &input.source)?;
        Ok(ParseOutput {
            language: self.language(),
            symbols,
        })
    }

    async fn symbols(&self, workspace: &Workspace) -> Result<SymbolIndex, AdapterError> {
        let root = workspace.root.clone();
        let files = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for file in collect_sources(&root, &["rs"]) {
                let Ok(source) = std::fs::read_to_string(&file) else {
                    continue;
                };
                let rel = rel_path(&root, &file);
                if let Ok(symbols) = codegraph::parse_symbols(&rel, &source) {
                    out.push((rel, symbols));
                }
            }
            out
        })
        .await
        .map_err(|e| AdapterError::Tool {
            tool: "spawn_blocking".into(),
            reason: e.to_string(),
        })?;
        Ok(SymbolIndex {
            language: self.language(),
            files,
        })
    }

    async fn diagnostics(&self, workspace: &Workspace) -> Result<Vec<Diagnostic>, AdapterError> {
        // Compiler diagnostics via `cargo check --message-format=json`. If cargo
        // is unavailable, degrade to an empty list rather than failing.
        if !on_path("cargo") {
            return Ok(Vec::new());
        }
        let output = tokio::process::Command::new("cargo")
            .args(["check", "--message-format=json", "--quiet"])
            .current_dir(&workspace.root)
            .output()
            .await?;
        Ok(parse_cargo_diagnostics(&output.stdout))
    }

    async fn build_metadata(&self, workspace: &Workspace) -> Result<BuildMetadata, AdapterError> {
        if !on_path("cargo") {
            return Ok(BuildMetadata::default());
        }
        let output = tokio::process::Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(&workspace.root)
            .output()
            .await?;
        if !output.status.success() {
            return Err(AdapterError::Tool {
                tool: "cargo metadata".into(),
                reason: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        parse_cargo_metadata(&output.stdout)
    }
}

/// Parse `cargo metadata --no-deps` JSON into [`BuildMetadata`].
fn parse_cargo_metadata(stdout: &[u8]) -> Result<BuildMetadata, AdapterError> {
    let value: serde_json::Value = serde_json::from_slice(stdout)?;
    let mut packages = Vec::new();
    let mut dependencies = Vec::new();
    if let Some(pkgs) = value.get("packages").and_then(|p| p.as_array()) {
        for pkg in pkgs {
            let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let version = pkg.get("version").and_then(|v| v.as_str()).unwrap_or("");
            packages.push(PackageInfo {
                name: name.to_string(),
                version: version.to_string(),
            });
            if let Some(deps) = pkg.get("dependencies").and_then(|d| d.as_array()) {
                for dep in deps {
                    if let Some(dn) = dep.get("name").and_then(|n| n.as_str()) {
                        dependencies.push(dn.to_string());
                    }
                }
            }
        }
    }
    dependencies.sort();
    dependencies.dedup();
    Ok(BuildMetadata {
        packages,
        dependencies,
    })
}

/// Parse `cargo check --message-format=json` output into diagnostics.
fn parse_cargo_diagnostics(stdout: &[u8]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for line in stdout.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let severity = match message.get("level").and_then(|l| l.as_str()) {
            Some("error") => DiagnosticSeverity::Error,
            Some("warning") => DiagnosticSeverity::Warning,
            Some("note") => DiagnosticSeverity::Info,
            _ => DiagnosticSeverity::Hint,
        };
        let text = message
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let (path, line) = message
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|s| s.get("is_primary") == Some(&serde_json::Value::Bool(true)))
            })
            .map(|span| {
                (
                    span.get("file_name")
                        .and_then(|f| f.as_str())
                        .unwrap_or("")
                        .to_string(),
                    span.get("line_start")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0) as u32,
                )
            })
            .unwrap_or_default();
        diagnostics.push(Diagnostic {
            path,
            line,
            severity,
            message: text,
        });
    }
    diagnostics
}

// --------------------------------------------------------------------------
// Python and TypeScript adapters (thinner, syntax-first)
// --------------------------------------------------------------------------

/// A thin, syntax-first adapter for a scripting language. It scans top-level
/// declarations line-by-line and reports [`SemanticCapability::SyntaxOnly`]
/// unless its language server is on `PATH`.
#[derive(Debug, Clone)]
pub struct ScriptAdapter {
    language: LanguageId,
    extensions: Vec<String>,
    language_server: String,
    scan: fn(&str) -> Vec<ParsedSymbol>,
}

impl ScriptAdapter {
    /// The Python adapter (`def`/`class` at module scope; pyright when present).
    #[must_use]
    pub fn python() -> Self {
        Self {
            language: LanguageId("python".into()),
            extensions: vec!["py".into()],
            language_server: "pyright".into(),
            scan: scan_python,
        }
    }

    /// The TypeScript/JavaScript adapter (`function`/`class`/`export`;
    /// typescript-language-server when present).
    #[must_use]
    pub fn typescript() -> Self {
        Self {
            language: LanguageId("typescript".into()),
            extensions: vec!["ts".into(), "tsx".into(), "js".into(), "jsx".into()],
            language_server: "typescript-language-server".into(),
            scan: scan_typescript,
        }
    }
}

#[async_trait]
impl LanguageAdapter for ScriptAdapter {
    fn language(&self) -> LanguageId {
        self.language.clone()
    }

    fn capability(&self) -> SemanticCapability {
        if on_path(&self.language_server) {
            SemanticCapability::LspResolved
        } else {
            SemanticCapability::SyntaxOnly
        }
    }

    async fn parse(&self, input: ParseInput) -> Result<ParseOutput, AdapterError> {
        Ok(ParseOutput {
            language: self.language(),
            symbols: (self.scan)(&input.source),
        })
    }

    async fn symbols(&self, workspace: &Workspace) -> Result<SymbolIndex, AdapterError> {
        let root = workspace.root.clone();
        let exts: Vec<String> = self.extensions.clone();
        let scan = self.scan;
        let files = tokio::task::spawn_blocking(move || {
            let ext_refs: Vec<&str> = exts.iter().map(String::as_str).collect();
            let mut out = Vec::new();
            for file in collect_sources(&root, &ext_refs) {
                let Ok(source) = std::fs::read_to_string(&file) else {
                    continue;
                };
                out.push((rel_path(&root, &file), scan(&source)));
            }
            out
        })
        .await
        .map_err(|e| AdapterError::Tool {
            tool: "spawn_blocking".into(),
            reason: e.to_string(),
        })?;
        Ok(SymbolIndex {
            language: self.language(),
            files,
        })
    }

    async fn diagnostics(&self, _workspace: &Workspace) -> Result<Vec<Diagnostic>, AdapterError> {
        // No LSP wired yet — graceful degradation to no diagnostics.
        Ok(Vec::new())
    }

    async fn build_metadata(&self, _workspace: &Workspace) -> Result<BuildMetadata, AdapterError> {
        Ok(BuildMetadata::default())
    }
}

/// Scan Python top-level `def`/`class` declarations (module scope: no indent).
fn scan_python(source: &str) -> Vec<ParsedSymbol> {
    use crate::types::CodeNodeKind;
    let mut out = Vec::new();
    for line in source.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("def ") {
            if let Some(name) = ident(rest) {
                out.push(ParsedSymbol {
                    qualified_name: name,
                    kind: CodeNodeKind::Function,
                    signature_hash: None,
                });
            }
        } else if let Some(rest) = trimmed.strip_prefix("class ") {
            if let Some(name) = ident(rest) {
                out.push(ParsedSymbol {
                    qualified_name: name,
                    kind: CodeNodeKind::Type,
                    signature_hash: None,
                });
            }
        }
    }
    out
}

/// Scan TypeScript/JavaScript top-level `function`/`class` declarations.
fn scan_typescript(source: &str) -> Vec<ParsedSymbol> {
    use crate::types::CodeNodeKind;
    let mut out = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let body = body.strip_prefix("default ").unwrap_or(body);
        if let Some(rest) = body.strip_prefix("function ") {
            if let Some(name) = ident(rest.trim_start_matches('*').trim_start()) {
                out.push(ParsedSymbol {
                    qualified_name: name,
                    kind: CodeNodeKind::Function,
                    signature_hash: None,
                });
            }
        } else if let Some(rest) = body.strip_prefix("class ") {
            if let Some(name) = ident(rest) {
                out.push(ParsedSymbol {
                    qualified_name: name,
                    kind: CodeNodeKind::Type,
                    signature_hash: None,
                });
            }
        }
    }
    out
}

/// The leading identifier of `s` (letters, digits, `_`), or `None`.
fn ident(s: &str) -> Option<String> {
    let name: String = s
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}
