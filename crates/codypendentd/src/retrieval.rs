//! The retrieval wiring the assembly owns (rubric 9): building the configured
//! embedding model, keeping the persisted vectors fresh, and serving the
//! agent-callable `skills.search` tool.
//!
//! Three seams meet here, and only here — knowledge defines them and stays
//! model-free (ADR-009), the runtime implements the provider half, and this
//! crate (the composition root, the only one that holds BOTH the pool and the
//! runtime) binds them together:
//!
//! 1. [`build_embedder`] turns `models.toml`'s `[embedding]` entry into a
//!    `SemanticEmbedder`; absent configuration uses the bundled local model.
//! 2. [`spawn_index_maintenance`] is the indexer worker `lib.rs` promised and
//!    never had: a startup reconcile plus a periodic
//!    [`drain_outbox`](codypendent_knowledge::drain_outbox) that finally
//!    CONSUMES the `index_outbox` (its `unprocessed`/`mark_processed` pair had
//!    zero production callers, so the table grew without bound).
//! 3. [`PoolRegistrySearch`] backs the runtime's `skills.search` tool over the
//!    same funnel `assemble_context` uses, and reads a disclosed skill's
//!    `SKILL.md` from its registered package directory.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use codypendent_integrations::mcp::McpBridge;
use codypendent_knowledge::{
    drain_outbox, reconcile_embeddings, retrieve, semantic_indexes, CapabilityRequest, Registry,
    RegistryItem, RegistryItemKind, RetrievalConfig, RetrievalQuery, RiskClass, Scope,
    SemanticEmbedder, ToolCard,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_runtime::embedding::BundledEmbedder;
use codypendent_runtime::models::{load_model_extras, RetrievalSettings};
use codypendent_runtime::tools::{
    RegistryCard, RegistrySearch, RegistrySearchOutcome, RegistrySearchRequest, SkillDocument,
    ToolSchema, SEARCH_CARD_LIMIT,
};
use codypendent_runtime::HttpEmbedder;
use sha2::{Digest as _, Sha256};
use sqlx::SqlitePool;
use tracing::{info, warn};

/// How many outbox rows one drain pass claims. Bounded so a backlog is worked
/// off over several passes instead of one long transaction-heavy burst.
const DRAIN_BATCH: i64 = 256;

/// How often the background drain runs. Registry writes are rare (a skill
/// install, a builtin re-registration), so a minute of staleness in a DERIVED
/// index costs nothing — and `semantic_indexes` embeds any still-missing item
/// inline at assembly time anyway, so retrieval is never actually stale.
const DRAIN_INTERVAL: Duration = Duration::from_secs(60);

/// Use an explicit provider override or the packaged local embedding model.
#[must_use]
pub fn build_embedder(paths: &RuntimePaths) -> Option<Arc<dyn SemanticEmbedder>> {
    let extras = match load_model_extras(&paths.data_dir.join("models.toml")) {
        Ok(extras) => extras,
        Err(error) => {
            warn!(%error, "could not read models.toml retrieval settings; using the offline embedder");
            return None;
        }
    };
    let Some(config) = extras.embedding else {
        return match BundledEmbedder::discover() {
            Ok(embedder) => Some(Arc::new(embedder)),
            Err(error) => {
                warn!(%error, "bundled semantic retrieval unavailable");
                None
            }
        };
    };
    match HttpEmbedder::from_config(&config) {
        Ok(embedder) => {
            info!(
                model = %config.model,
                endpoint = %embedder.endpoint(),
                "semantic embeddings enabled for retrieval"
            );
            Some(Arc::new(embedder))
        }
        Err(error) => {
            warn!(%error, "invalid [embedding] entry; retrieval keeps the offline hashing embedder");
            None
        }
    }
}

/// The `[retrieval]` tuning (today: `mcp_top_k`). An unreadable/malformed file
/// yields the defaults — the same degrade-don't-fail stance as
/// [`build_embedder`].
#[must_use]
pub fn retrieval_settings(paths: &RuntimePaths) -> RetrievalSettings {
    load_model_extras(&paths.data_dir.join("models.toml"))
        .map(|extras| extras.retrieval)
        .unwrap_or_default()
}

/// Start the index-maintenance job: one reconcile pass, then a drain every
/// [`DRAIN_INTERVAL`], for the life of the process.
///
/// The reconcile exists because the outbox is a queue, not a log: rows written
/// while no embedding model was configured are consumed by a drain that had
/// nothing to persist, so a later `[embedding]` entry would strand every
/// existing item. A full-scan pass at startup (and thus after any config change,
/// which requires a restart) backfills them.
///
/// Fire-and-forget, like the code-graph warm-up: a failure is logged and the
/// next pass retries. Nothing here is on any request path — `assemble_context`
/// embeds whatever is still missing inline — so a wedged job degrades cost, not
/// correctness.
pub fn spawn_index_maintenance(pool: SqlitePool, embedder: Option<Arc<dyn SemanticEmbedder>>) {
    tokio::spawn(async move {
        let mut reconciled_space = String::new();
        loop {
            if let Some(embedder) = &embedder {
                if reconciled_space != embedder.space_id() {
                    match reconcile_embeddings(&pool, embedder.as_ref()).await {
                        Ok(written) => {
                            reconciled_space = embedder.space_id().to_string();
                            info!(written, "registry semantic embeddings current");
                        }
                        Err(error) => warn!(%error, "semantic index warm-up deferred"),
                    }
                }
            }
            match drain_outbox(&pool, embedder.as_deref(), DRAIN_BATCH).await {
                Ok(report) if report.processed == 0 => {}
                Ok(report) => info!(
                    processed = report.processed,
                    embedded = report.embedded,
                    deleted = report.deleted,
                    deferred = report.deferred_kinds,
                    "drained the knowledge index outbox"
                ),
                Err(error) => warn!(%error, "index outbox drain failed; retrying next pass"),
            }
            let interval = if embedder.is_some() && reconciled_space.is_empty() {
                Duration::from_secs(2)
            } else {
                DRAIN_INTERVAL
            };
            tokio::time::sleep(interval).await;
        }
    });
}

/// The `skills.search` backend: the retrieval funnel over the knowledge pool,
/// plus the package read that turns a disclosed skill card into its procedure.
#[derive(Clone)]
pub struct PoolRegistrySearch {
    pool: SqlitePool,
    embedder: Option<Arc<dyn SemanticEmbedder>>,
    mcp: Option<Arc<dyn McpBridge>>,
}

impl PoolRegistrySearch {
    /// Bind a search backend to the knowledge pool and (optionally) the
    /// configured embedding model — the same pair `assemble_context` uses, so an
    /// agent's own search ranks by exactly what the system-assembled context
    /// ranked by.
    #[must_use]
    pub fn new(pool: SqlitePool, embedder: Option<Arc<dyn SemanticEmbedder>>) -> Self {
        Self {
            pool,
            embedder,
            mcp: None,
        }
    }

    pub fn with_mcp_catalog(mut self, mcp: Option<Arc<dyn McpBridge>>) -> Self {
        self.mcp = mcp;
        self
    }

    /// Read a registered skill's `SKILL.md`.
    ///
    /// The package directory comes from the item's own `Provenance::Package`
    /// row and the file name from its validated manifest — never from the model.
    /// The package is revalidated against the exact registered content hash and
    /// every path component is opened relative to a pinned package-directory
    /// descriptor with `O_NOFOLLOW`. Registration-time validation alone is not
    /// enough: a local process could otherwise replace the manifest or retarget
    /// a symlink after registration and turn `open` into an arbitrary-file read.
    /// Returns `Err(note)` with a legible reason the tool renders rather than
    /// swallowing.
    fn read_skill_document(item: &RegistryItem) -> Result<SkillDocument, String> {
        let codypendent_knowledge::Provenance::Package { path } = &item.provenance else {
            return Err(format!(
                "`{}` is a built-in, not a skill package with a written procedure",
                item.name
            ));
        };
        let dir = PathBuf::from(path);
        let root = open_package_root(&dir)
            .map_err(|error| format!("could not secure `{}`'s package: {error}", item.name))?;
        let snapshot = snapshot_open_package(&root)
            .map_err(|error| format!("could not verify `{}`'s package: {error}", item.name))?;
        if snapshot.content_hash != item.content_hash {
            return Err(format!(
                "`{}` changed after registration; re-register it before opening",
                item.name
            ));
        }
        let instructions = if snapshot.files.contains_key("skill.toml") {
            let manifest_text = snapshot
                .text(Path::new("skill.toml"))
                .map_err(|error| format!("could not read `{}`'s manifest: {error}", item.name))?;
            let manifest: codypendent_knowledge::SkillManifest = toml::from_str(&manifest_text)
                .map_err(|error| format!("could not parse `{}`'s manifest: {error}", item.name))?;
            if manifest.id != item.name || manifest.version != item.version.0 {
                return Err(format!(
                    "`{}` no longer matches its registered identity",
                    item.name
                ));
            }
            manifest
                .entrypoints
                .instructions
                .ok_or_else(|| format!("`{}` declares no instructions entrypoint", item.name))?
        } else {
            let slug = dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "skill".to_string());
            if item.name != slug || item.version.0 != "0.0.0" {
                return Err(format!(
                    "`{}` no longer matches its registered identity",
                    item.name
                ));
            }
            "SKILL.md".to_string()
        };
        let content = snapshot
            .text(Path::new(&instructions))
            .map_err(|error| format!("could not read `{}`: {error}", instructions))?;
        Ok(SkillDocument {
            name: item.name.clone(),
            content,
        })
    }
}

const MAX_OPEN_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(unix)]
struct OpenPackageFile {
    relative: String,
    file: std::fs::File,
    size: u64,
}

struct OpenPackageSnapshot {
    content_hash: String,
    files: HashMap<String, Vec<u8>>,
}

impl OpenPackageSnapshot {
    fn text(&self, relative: &Path) -> std::io::Result<String> {
        let mut normalized = String::new();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "package path must be relative and may not contain `..`",
                ));
            };
            let name = name.to_str().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "package path is not valid UTF-8",
                )
            })?;
            if !normalized.is_empty() {
                normalized.push('/');
            }
            normalized.push_str(name);
        }
        let bytes = self.files.get(&normalized).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("package file `{normalized}` is missing"),
            )
        })?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
            })
    }
}

/// Recompute the registry's deterministic package digest entirely through the
/// pinned directory descriptor. This closes the validate/open race: replacing
/// the path while the tool runs cannot change which tree or regular-file
/// descriptors are read.
#[cfg(unix)]
fn snapshot_open_package(root: &std::fs::File) -> std::io::Result<OpenPackageSnapshot> {
    let mut files = Vec::new();
    collect_open_package_files(root, "", &mut files)?;
    files.sort_by(|left, right| left.relative.cmp(&right.relative));

    let mut hasher = Sha256::new();
    let mut contents = HashMap::new();
    let mut total = 0_u64;
    let mut chunk = vec![0_u8; 64 * 1024];
    for mut entry in files {
        total = total.saturating_add(entry.size);
        if total > MAX_OPEN_PACKAGE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "skill package exceeds the 64 MiB read ceiling",
            ));
        }
        hasher.update((entry.relative.len() as u64).to_le_bytes());
        hasher.update(entry.relative.as_bytes());
        hasher.update(entry.size.to_le_bytes());
        let mut bytes = Vec::with_capacity(usize::try_from(entry.size).unwrap_or_default());
        loop {
            let count = entry.file.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            hasher.update(&chunk[..count]);
            bytes.extend_from_slice(&chunk[..count]);
        }
        contents.insert(entry.relative, bytes);
    }
    Ok(OpenPackageSnapshot {
        content_hash: hex::encode(hasher.finalize()),
        files: contents,
    })
}

#[cfg(unix)]
fn collect_open_package_files(
    directory: &std::fs::File,
    prefix: &str,
    out: &mut Vec<OpenPackageFile>,
) -> std::io::Result<()> {
    use rustix::fs::{openat, statat, AtFlags, Dir, FileType, Mode, OFlags};

    let mut entries = Dir::read_from(directory).map_err(std::io::Error::from)?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let name_text = name.to_str().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "package path is not valid UTF-8",
            )
        })?;
        let relative = if prefix.is_empty() {
            name_text.to_owned()
        } else {
            format!("{prefix}/{name_text}")
        };
        let stat =
            statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
        let file_type = FileType::from_raw_mode(stat.st_mode);
        if file_type.is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("package contains symlink `{relative}`"),
            ));
        }
        if file_type.is_dir() {
            let fd = openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            let child = std::fs::File::from(fd);
            collect_open_package_files(&child, &relative, out)?;
        } else if file_type.is_file() {
            let fd = openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            let file = std::fs::File::from(fd);
            let size = file.metadata()?.len();
            out.push(OpenPackageFile {
                relative,
                file,
                size,
            });
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("package contains non-regular entry `{relative}`"),
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn snapshot_open_package(_root: &std::fs::File) -> std::io::Result<OpenPackageSnapshot> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure skill package verification is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn open_package_root(dir: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{open, Mode, OFlags};

    let canonical = dir.canonicalize()?;
    let fd = open(
        canonical,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(std::fs::File::from(fd))
}

#[cfg(not(unix))]
fn open_package_root(dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(dir.canonicalize()?)
}

/// Project a disclosed card + its authoritative item into the tool's card shape.
/// Permissions come from the ITEM (the card carries only name/summary/risk), so
/// the model sees what selecting the item would actually cost.
fn to_registry_card(card: &ToolCard, item: Option<&RegistryItem>) -> RegistryCard {
    RegistryCard {
        name: card.name.clone(),
        kind: match card.kind {
            RegistryItemKind::Tool => "tool",
            RegistryItemKind::Skill => "skill",
            RegistryItemKind::Plugin => "plugin",
            RegistryItemKind::Hook => "hook",
            RegistryItemKind::Command => "command",
        }
        .to_string(),
        summary: card.summary.clone(),
        permissions: item
            .map(|item| {
                item.permissions
                    .iter()
                    .map(render_permission)
                    .chain(crate::retrieval_catalog::policy_marker(item))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// One declared capability as `kind:target` — the shape the approval card and
/// the policy engine speak, so what the model reads here matches what it will
/// later be asked to approve.
fn render_permission(request: &CapabilityRequest) -> String {
    match request {
        CapabilityRequest::FilesystemRead(target) => format!("filesystem-read:{target}"),
        CapabilityRequest::FilesystemWrite(target) => format!("filesystem-write:{target}"),
        CapabilityRequest::Command(target) => format!("command:{target}"),
        CapabilityRequest::Network(target) => format!("network:{target}"),
        CapabilityRequest::Secret(target) => format!("secret:{target}"),
    }
}

#[async_trait]
impl RegistrySearch for PoolRegistrySearch {
    async fn search(
        &self,
        request: RegistrySearchRequest<'_>,
    ) -> Result<RegistrySearchOutcome, String> {
        let repository = crate::scan::repository_id_for(request.repository);
        let mut items = Registry::new()
            .list(&self.pool)
            .await
            .map_err(|error| format!("could not read the registry: {error}"))?;
        let catalog = crate::retrieval_catalog::offered_items(self.mcp.as_deref());
        let (indexes, query_vector) = if catalog.is_empty() {
            semantic_indexes(&self.pool, &items, self.embedder.as_deref(), request.query).await
        } else {
            items.extend(catalog);
            codypendent_runtime::tool_retrieval::tool_indexes(
                &items,
                request.query,
                self.embedder.as_deref(),
            )
            .await
        }
        .map_err(|error| format!("could not build the retrieval indexes: {error}"))?;

        // The SAME visibility and risk ceiling `assemble_context` applies: an
        // agent-initiated search can never see more than the system-assembled
        // context would have disclosed for this repository.
        let mut query = RetrievalQuery::new(
            request.query,
            // The same three scopes context assembly discloses. Omitting the
            // local user scope made a globally installed skill visible as a
            // context card yet unreachable through `skills.search` — it would
            // report the skill "not disclosed" and never open its SKILL.md.
            vec![
                Scope::System,
                codypendent_knowledge::local_user_scope(),
                Scope::Repository(repository),
            ],
            RiskClass::Medium,
        );
        query.query_vector = query_vector;
        let config = RetrievalConfig {
            disclose_tools_max: SEARCH_CARD_LIMIT,
            ..RetrievalConfig::default()
        };
        let result = retrieve(&items, &indexes, &query, &config)
            .map_err(|error| format!("retrieval failed: {error}"))?;

        let by_id: std::collections::HashMap<_, _> =
            items.iter().map(|item| (item.id, item)).collect();
        // Commands lead: `to_registry_card`'s `Command => "command"` arm was
        // unreachable until `retrieve` started returning them, and a prepared
        // `/fix-ci` is the most useful single line a search for "the ci is red"
        // can return — more useful than the eighth tool card.
        let cards: Vec<RegistryCard> = result
            .commands
            .iter()
            .chain(result.skills.iter())
            .chain(result.tools.iter())
            .map(|card| to_registry_card(card, by_id.get(&card.id).copied()))
            .collect();

        let (document, tool_schema, open_note) = match request.open {
            None => (None, None, None),
            Some(name) => {
                if let Some(item) = result
                    .tools
                    .iter()
                    .find(|card| card.name == name)
                    .and_then(|card| by_id.get(&card.id).copied())
                {
                    match &item.input_schema {
                        Some(parameters) => (
                            None,
                            Some(ToolSchema {
                                name: name.to_string(),
                                parameters: parameters.clone(),
                            }),
                            None,
                        ),
                        None => (
                            None,
                            None,
                            Some(format!("`{name}` has no registered input schema")),
                        ),
                    }
                } else {
                    let disclosed = result
                        .skills
                        .iter()
                        .find(|card| card.name == name)
                        .and_then(|card| by_id.get(&card.id).copied());
                    match disclosed {
                        None => (
                            None,
                            None,
                            Some(format!(
                                "`{name}` is not among the skills or tools this search disclosed"
                            )),
                        ),
                        Some(item) => match Self::read_skill_document(item) {
                            Ok(document) => (Some(document), None, None),
                            Err(note) => (None, None, Some(note)),
                        },
                    }
                }
            }
        };

        Ok(RegistrySearchOutcome {
            cards,
            document,
            tool_schema,
            open_note,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_knowledge::db;
    use std::path::Path;

    /// A paths bundle whose `data_dir` is `dir`; the rest are unused by the
    /// retrieval readers, which only ever open `<data_dir>/models.toml`.
    fn paths_at(dir: &Path) -> RuntimePaths {
        RuntimePaths {
            data_dir: dir.to_path_buf(),
            config_dir: dir.to_path_buf(),
            run_dir: dir.to_path_buf(),
            socket_path: dir.join("sock"),
            pid_path: dir.join("pid"),
            log_dir: dir.to_path_buf(),
        }
    }

    /// A minimal on-disk skill package: `skill.toml` + the `SKILL.md` its
    /// `[entrypoints]` declares.
    fn write_package(dir: &Path, name: &str, description: &str, procedure: &str) {
        std::fs::create_dir_all(dir).expect("package dir");
        std::fs::write(
            dir.join("skill.toml"),
            format!(
                r#"
schema_version = 1
id = "{name}"
name = "Fix CI"
version = "1.0.0"
scope = "system"
status = "active"
description = "{description}"
intents = ["fix a red continuous integration build"]

[entrypoints]
instructions = "SKILL.md"

[permissions]
commands = ["cargo"]

[trust]
publisher = "local-user"
"#
            ),
        )
        .expect("write manifest");
        std::fs::write(dir.join("SKILL.md"), procedure).expect("write procedure");
    }

    /// The `skills.search` seam end to end over a real pool: the funnel
    /// discloses the registered skill for a matching query, its card carries the
    /// declared permissions, and `open` returns the package's `SKILL.md`.
    #[tokio::test]
    async fn search_discloses_a_registered_skill_and_opens_its_procedure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open(&tmp.path().join("codypendent.db"))
            .await
            .expect("pool");
        codypendent_knowledge::register_builtins(&pool)
            .await
            .expect("builtins");

        let package = tmp.path().join("fix-ci");
        write_package(
            &package,
            "rust.fix-ci",
            "diagnose and fix a failing continuous integration build",
            "1. read the failing job log\n2. reproduce locally with cargo test\n",
        );
        Registry::new()
            .register_package(&pool, &package, Scope::System)
            .await
            .expect("registers");

        let search = PoolRegistrySearch::new(pool.clone(), None);
        let outcome = search
            .search(RegistrySearchRequest {
                query: "the continuous integration build is failing",
                open: Some("rust.fix-ci"),
                repository: tmp.path(),
            })
            .await
            .expect("searches");

        let skill = outcome
            .cards
            .iter()
            .find(|card| card.name == "rust.fix-ci")
            .unwrap_or_else(|| panic!("the skill must be disclosed: {:?}", outcome.cards));
        assert_eq!(skill.kind, "skill");
        assert_eq!(
            skill.permissions,
            vec!["command:cargo".to_string()],
            "the card carries what selecting the skill would cost"
        );
        let document = outcome.document.expect("open returns the procedure");
        assert_eq!(document.name, "rust.fix-ci");
        assert!(document
            .content
            .contains("reproduce locally with cargo test"));
        assert_eq!(outcome.open_note, None);
    }

    #[tokio::test]
    async fn ecosystem_markdown_skills_open_securely_and_missing_packages_stop_ranking() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::open(&tmp.path().join("ecosystem.db")).await.unwrap();
        let package = tmp.path().join("azure-resource-visualizer");
        std::fs::create_dir_all(package.join("references")).unwrap();
        std::fs::create_dir_all(package.join("assets")).unwrap();
        let procedure = "---\nname: Display title\ndescription: Visualise Azure resources as a knowledge graph.\n---\n\nRead the resource inventory and draw its connections.\n";
        std::fs::write(package.join("SKILL.md"), procedure).unwrap();
        std::fs::write(
            package.join("references/guide.md"),
            "Use the current inventory.",
        )
        .unwrap();
        std::fs::write(package.join("assets/example.txt"), "graph example").unwrap();
        let repository = crate::scan::repository_id_for(tmp.path());
        let item = Registry::new()
            .register_package(&pool, &package, Scope::Repository(repository))
            .await
            .unwrap();
        assert_eq!(item.name, "azure-resource-visualizer");
        assert_eq!(item.version.0, "0.0.0");
        let search = PoolRegistrySearch::new(pool.clone(), None);
        let request = RegistrySearchRequest {
            query: "Visualise Azure resources as a knowledge graph",
            open: Some("azure-resource-visualizer"),
            repository: tmp.path(),
        };
        let opened = search.search(request).await.unwrap();
        assert_eq!(opened.document.unwrap().content, procedure);
        assert_eq!(opened.open_note, None);

        let mut wrong_identity = item.clone();
        wrong_identity.name = "wrong-slug".to_string();
        assert!(PoolRegistrySearch::read_skill_document(&wrong_identity)
            .unwrap_err()
            .contains("registered identity"));
        std::fs::write(package.join("assets/example.txt"), "changed").unwrap();
        assert!(PoolRegistrySearch::read_skill_document(&item)
            .unwrap_err()
            .contains("changed after registration"));
        std::fs::write(package.join("assets/example.txt"), "graph example").unwrap();

        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside.md");
            std::fs::write(&outside, "DO NOT DISCLOSE").unwrap();
            std::fs::remove_file(package.join("SKILL.md")).unwrap();
            std::os::unix::fs::symlink(outside, package.join("SKILL.md")).unwrap();
            let error = PoolRegistrySearch::read_skill_document(&item).unwrap_err();
            assert!(error.contains("symlink"));
            assert!(!error.contains("DO NOT DISCLOSE"));
            std::fs::remove_file(package.join("SKILL.md")).unwrap();
            std::fs::write(package.join("SKILL.md"), procedure).unwrap();
        }

        let context = codypendent_knowledge::ContextAssembler::new();
        let before = context
            .assemble(&pool, repository, request.query, &[])
            .await
            .unwrap();
        assert!(before.skill_cards.iter().any(|card| card.name == item.name));
        std::fs::rename(&package, tmp.path().join("removed-package")).unwrap();
        let gone = search.search(request).await.unwrap();
        assert!(gone.cards.iter().all(|card| card.name != item.name));
        assert!(gone.document.is_none());
        let after = context
            .assemble(&pool, repository, request.query, &[])
            .await
            .unwrap();
        assert!(after.skill_cards.iter().all(|card| card.name != item.name));
        assert_eq!(
            Registry::new()
                .get(&pool, item.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            codypendent_knowledge::RegistryStatus::Active
        );
    }

    #[tokio::test]
    async fn opening_rejects_a_package_mutated_after_registration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open(&tmp.path().join("codypendent.db"))
            .await
            .expect("pool");
        let package = tmp.path().join("fix-ci");
        write_package(
            &package,
            "rust.fix-ci",
            "diagnose a failing continuous integration build",
            "registered procedure",
        );
        Registry::new()
            .register_package(&pool, &package, Scope::System)
            .await
            .expect("registers");
        let item = Registry::new()
            .list(&pool)
            .await
            .expect("list")
            .into_iter()
            .find(|item| item.name == "rust.fix-ci")
            .expect("registered item");

        std::fs::write(tmp.path().join("outside-secret.md"), "DO NOT DISCLOSE")
            .expect("outside file");
        let manifest = std::fs::read_to_string(package.join("skill.toml"))
            .expect("read manifest")
            .replace(
                "instructions = \"SKILL.md\"",
                "instructions = \"../outside-secret.md\"",
            );
        std::fs::write(package.join("skill.toml"), manifest).expect("retarget manifest");

        let error = PoolRegistrySearch::read_skill_document(&item)
            .expect_err("post-registration escape must fail closed");
        assert!(
            error.contains("changed after registration") || error.contains("escape"),
            "unexpected refusal: {error}"
        );
        assert!(!error.contains("DO NOT DISCLOSE"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opening_rejects_a_post_registration_instruction_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open(&tmp.path().join("codypendent.db"))
            .await
            .expect("pool");
        let package = tmp.path().join("fix-ci");
        write_package(
            &package,
            "rust.fix-ci",
            "diagnose a failing continuous integration build",
            "registered procedure",
        );
        Registry::new()
            .register_package(&pool, &package, Scope::System)
            .await
            .expect("registers");
        let item = Registry::new()
            .list(&pool)
            .await
            .expect("list")
            .into_iter()
            .find(|item| item.name == "rust.fix-ci")
            .expect("registered item");

        let secret = tmp.path().join("outside-secret.md");
        std::fs::write(&secret, "DO NOT DISCLOSE").expect("outside file");
        std::fs::remove_file(package.join("SKILL.md")).expect("remove instructions");
        symlink(&secret, package.join("SKILL.md")).expect("replace with symlink");

        let error = PoolRegistrySearch::read_skill_document(&item)
            .expect_err("a post-registration symlink must fail closed");
        assert!(
            error.contains("symlink") || error.contains("changed after registration"),
            "unexpected refusal: {error}"
        );
        assert!(!error.contains("DO NOT DISCLOSE"));
    }

    /// `open` resolves ONLY against the skills this search disclosed, so it can
    /// never be used to read a package the hard filters excluded — and the
    /// refusal is reported rather than silently dropped.
    #[tokio::test]
    async fn open_is_limited_to_disclosed_skills_and_says_why_when_it_is_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open(&tmp.path().join("codypendent.db"))
            .await
            .expect("pool");
        codypendent_knowledge::register_builtins(&pool)
            .await
            .expect("builtins");

        let search = PoolRegistrySearch::new(pool, None);
        let outcome = search
            .search(RegistrySearchRequest {
                query: "read a file",
                open: Some("rust.fix-ci"),
                repository: tmp.path(),
            })
            .await
            .expect("searches");
        assert!(outcome.document.is_none());
        assert!(
            outcome
                .open_note
                .as_deref()
                .is_some_and(|note| note.contains("not among the skills")),
            "an undisclosed name is reported: {:?}",
            outcome.open_note
        );
        assert!(
            !outcome.cards.is_empty(),
            "the cards themselves are still returned"
        );
    }

    /// A built-in has no package on disk, so `open` on one is a legible note
    /// rather than a filesystem error.
    #[tokio::test]
    async fn opening_a_builtin_is_a_legible_note() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open(&tmp.path().join("codypendent.db"))
            .await
            .expect("pool");
        codypendent_knowledge::register_builtins(&pool)
            .await
            .expect("builtins");
        let item = Registry::new()
            .list(&pool)
            .await
            .expect("list")
            .into_iter()
            .find(|item| item.name == "workspace.read_file")
            .expect("a builtin exists");
        assert!(PoolRegistrySearch::read_skill_document(&item)
            .expect_err("a builtin ships no procedure")
            .contains("built-in"));
    }

    #[test]
    fn explicit_embedding_settings_override_the_bundled_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = paths_at(tmp.path());
        assert!(build_embedder(&paths).is_none());
        assert_eq!(
            retrieval_settings(&paths),
            RetrievalSettings::default(),
            "an absent models.toml yields the default tuning, never a failure"
        );

        // A configured entry builds an embedder without touching the network.
        std::fs::write(
            tmp.path().join("models.toml"),
            "[embedding]\nbase_url = \"http://localhost:11434/v1\"\nmodel = \"nomic-embed-text\"\n\n[retrieval]\nmcp_top_k = 5\n",
        )
        .expect("write models.toml");
        let embedder = build_embedder(&paths).expect("builds from a valid entry");
        assert_eq!(embedder.model(), "nomic-embed-text");
        assert_eq!(retrieval_settings(&paths).mcp_top_k, 5);
    }
}
