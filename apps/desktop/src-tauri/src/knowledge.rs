//! The knowledge surfaces' data, read the way the terminal client reads it.
//!
//! Skills, memories, learnings and documents live in the daemon's SQLite
//! database (`<data_dir>/codypendent.db`); there is no wire command that
//! lists them, so every client opens the database itself. This module is the
//! desktop's copy of the mapping `crates/cli/src/tui.rs` performs for the TUI:
//! the same stores, the same visible scopes, the same card shapes
//! (`apps/desktop/src/components/knowledgeTransport.ts` mirrors the TUI's
//! projections one-for-one), so the two clients show the same facts about the
//! same database.
//!
//! Reads open the database READ-ONLY (`db::open_read_only`): a shell that
//! merely inspects must neither create a database out of the act of asking
//! whether one exists, nor migrate the daemon's live file underneath it. The
//! one local write, a learning mutation, opens it exactly as the TUI does.
//!
//! The daemon-owned operations (correcting or forgetting a memory, editing or
//! publishing a document, the Remote UI plugin lifecycle) are protocol
//! commands and live on [`crate::daemon::DaemonClient`], not here.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use codypendent_knowledge::{
    anchor_repository_id, db as knowledge_db, ActivationOutcome, BlockContent, CapabilityRequest,
    CollaborationMode, DocumentAuthor, DocumentBlock, DocumentStore, EvidenceRef,
    KnowledgeDocument, LearningContent, LearningKind, LearningMutationOutcome, LearningPatch,
    LearningProvenance, LearningQuery, LearningRecord, LearningScope, LearningState, LearningStore,
    MemoryClass, MemoryRecord, MemoryStore, Registry, RegistryItem, RegistryItemKind,
    RegistryStatus, RiskClass, Scope, Suggestion, SuggestionStatus, SuggestionStore, TrustTier,
    Verification,
};
use codypendent_protocol::{LearningId, UserId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// The file name the daemon and every client agree on.
const DATABASE_FILE: &str = "codypendent.db";

/// Who is asking: the scopes a read is allowed to see.
///
/// Mirrors the TUI's visible set — the System tier, this connection's
/// workspace, and the selected repository, where a run's harvested memories
/// and documents live. Either identity can be absent (no daemon connection
/// yet, no repository chosen) and the read then simply sees less; the stores
/// enforce cross-scope isolation in SQL, so an empty result is a real answer.
#[derive(Debug, Clone, Default)]
pub struct KnowledgeIdentity {
    /// The workspace the current daemon connection carries, when connected.
    pub workspace: Option<WorkspaceId>,
    /// The checkout the operator selected on the Repository page, when any.
    pub repository: Option<PathBuf>,
}

/// `crates/tui/src/state.rs` `SkillCard`. `permissions` are verbatim strings.
#[derive(Debug, Clone, Serialize)]
pub struct SkillCard {
    pub name: String,
    pub kind: String,
    pub scope: String,
    pub trust: String,
    pub status: String,
    pub risk: String,
    pub description: String,
    pub permissions: Vec<String>,
}

/// `crates/tui/src/state.rs` `MemoryCard`, plus the memory's id so the
/// desktop can address `CorrectMemory`/`ForgetMemory` at it.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryCard {
    pub id: String,
    pub statement: String,
    pub class: String,
    pub scope: String,
    pub revision: String,
    /// A date string, exactly as the store rendered it.
    pub observed: String,
    pub confidence: f32,
    /// The human-readable evidence source; "(no evidence)" when there is none.
    pub source: String,
}

/// `crates/tui/src/state.rs` `LearningCard`.
#[derive(Debug, Clone, Serialize)]
pub struct LearningCard {
    pub id: String,
    pub statement: String,
    pub kind: String,
    pub state: String,
    pub scope: String,
    pub provenance: String,
    pub confidence: f32,
    pub pinned: bool,
    pub revision: u64,
}

/// `crates/tui/src/action.rs` `LearningMutation`, in the shape the webview
/// sends (`{ "type": "SetPinned", "pinned": true }`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum LearningMutation {
    Activate,
    Reject,
    SetPinned { pinned: bool },
    EditStatement { statement: String },
    Delete,
}

/// `crates/tui/src/state.rs` `DocBlockView`.
#[derive(Debug, Clone, Serialize)]
pub struct DocBlockView {
    pub id: String,
    pub kind: String,
    /// A one-line, lossy display rendering.
    pub text: String,
    /// The block's primary text verbatim, or `None` for a structured block
    /// with no single editable container.
    pub editable: Option<String>,
}

/// `crates/tui/src/state.rs` `DocSuggestionView`.
#[derive(Debug, Clone, Serialize)]
pub struct DocSuggestionView {
    pub id: String,
    pub block_id: String,
    pub source_revision: u64,
    pub status: String,
    pub author: String,
    pub range: String,
    pub original: String,
    pub replacement: String,
    pub rationale: Option<String>,
}

/// `crates/tui/src/state.rs` `DocCard`.
#[derive(Debug, Clone, Serialize)]
pub struct DocCard {
    pub document_id: String,
    pub title: String,
    pub scope: String,
    pub status: String,
    pub mode: String,
    /// Pre-rendered, e.g. `"r7"`.
    pub revision: String,
    pub blocks: Vec<DocBlockView>,
    pub suggestions: Vec<DocSuggestionView>,
}

// ---------------------------------------------------------------------------
// Opening the database
// ---------------------------------------------------------------------------

fn database_path() -> anyhow::Result<PathBuf> {
    let paths = crate::runtime_paths::resolve().context("resolving codypendent runtime paths")?;
    Ok(paths.data_dir.join(DATABASE_FILE))
}

/// Open the database for reading, or say precisely why that is not possible.
///
/// A missing file is the common first-run case and gets its own sentence: the
/// daemon creates the database the first time it runs, and nothing the
/// desktop could do here would be right to do instead.
async fn read_pool_at(path: &Path) -> anyhow::Result<SqlitePool> {
    if !path.exists() {
        bail!(
            "no knowledge database at {} yet — codypendentd creates it the first time it runs",
            path.display()
        );
    }
    knowledge_db::open_read_only(path)
        .await
        .with_context(|| format!("opening {} read-only", path.display()))
}

async fn read_pool() -> anyhow::Result<SqlitePool> {
    read_pool_at(&database_path()?).await
}

/// Open the database for the one local write, exactly as the TUI does.
async fn write_pool() -> anyhow::Result<SqlitePool> {
    let path = database_path()?;
    knowledge_db::open(&path)
        .await
        .with_context(|| format!("opening {}", path.display()))
}

/// The memory/document scopes an identity may see (`load_knowledge` in
/// `crates/cli/src/tui.rs`). The repository identity is the anchored
/// checkout's — resolving the Git toplevel first, exactly as the daemon's
/// `repository_id_for` does — so a repository selected by a subdirectory
/// still sees its own memories.
fn memory_scopes(identity: &KnowledgeIdentity) -> Vec<Scope> {
    let mut scopes = vec![Scope::System];
    if let Some(workspace) = identity.workspace {
        scopes.push(Scope::Workspace(workspace));
    }
    if let Some(repository) = &identity.repository {
        scopes.push(Scope::Repository(anchor_repository_id(repository)));
    }
    scopes
}

/// The learning scopes an identity may see (`load_journey` in the TUI).
fn learning_scopes(identity: &KnowledgeIdentity) -> Vec<LearningScope> {
    let mut scopes = vec![LearningScope::User(local_user())];
    if let Some(repository) = &identity.repository {
        scopes.push(LearningScope::Repository(anchor_repository_id(repository)));
    }
    scopes
}

fn local_user() -> UserId {
    UserId("local".to_owned())
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Every governed registry item, as the Skill Studio shows it.
pub async fn list_skills() -> anyhow::Result<Vec<SkillCard>> {
    let pool = read_pool().await?;
    let result = skills_in(&pool).await;
    pool.close().await;
    result
}

async fn skills_in(pool: &SqlitePool) -> anyhow::Result<Vec<SkillCard>> {
    let items = Registry::new()
        .list(pool)
        .await
        .context("listing registry items")?;
    Ok(items.iter().map(skill_card).collect())
}

/// The live memories visible to `identity`.
pub async fn list_memories(identity: &KnowledgeIdentity) -> anyhow::Result<Vec<MemoryCard>> {
    let pool = read_pool().await?;
    let result = memories_in(&pool, identity).await;
    pool.close().await;
    result
}

async fn memories_in(
    pool: &SqlitePool,
    identity: &KnowledgeIdentity,
) -> anyhow::Result<Vec<MemoryCard>> {
    let records = MemoryStore::new()
        .query(pool, &memory_scopes(identity), None)
        .await
        .context("querying memories")?;
    Ok(records.iter().map(memory_card).collect())
}

/// The proposed and active learnings visible to `identity`.
pub async fn list_learnings(identity: &KnowledgeIdentity) -> anyhow::Result<Vec<LearningCard>> {
    let pool = read_pool().await?;
    let result = learnings_in(&pool, identity).await;
    pool.close().await;
    result
}

async fn learnings_in(
    pool: &SqlitePool,
    identity: &KnowledgeIdentity,
) -> anyhow::Result<Vec<LearningCard>> {
    let records = LearningStore::new()
        .query(
            pool,
            &LearningQuery {
                scopes: learning_scopes(identity),
                states: vec![LearningState::Proposed, LearningState::Active],
                ..LearningQuery::default()
            },
        )
        .await
        .context("querying learnings")?;
    Ok(records.iter().map(learning_card).collect())
}

/// Every document visible to `identity`, with its blocks and pending
/// suggestions.
///
/// A document whose snapshot cannot be read is skipped, as in the TUI, so one
/// damaged document does not hide the rest — unless it hid ALL of them, in
/// which case the read fails with what went wrong rather than answering with
/// a plausible empty list.
pub async fn list_documents(identity: &KnowledgeIdentity) -> anyhow::Result<Vec<DocCard>> {
    let pool = read_pool().await?;
    let result = documents_in(&pool, identity).await;
    pool.close().await;
    result
}

async fn documents_in(
    pool: &SqlitePool,
    identity: &KnowledgeIdentity,
) -> anyhow::Result<Vec<DocCard>> {
    let doc_store = DocumentStore::new();
    let suggestion_store = SuggestionStore::new();
    let summaries = doc_store
        .list(pool, &memory_scopes(identity))
        .await
        .context("listing documents")?;
    let mut docs = Vec::with_capacity(summaries.len());
    let mut failures = Vec::new();
    for summary in summaries {
        let document = match doc_store.snapshot_document(pool, summary.id).await {
            Ok(Some(document)) => document,
            Ok(None) => continue,
            Err(error) => {
                failures.push(format!("document {}: {error}", summary.id));
                continue;
            }
        };
        let suggestions = match suggestion_store.pending(pool, summary.id).await {
            Ok(suggestions) => suggestions,
            Err(error) => {
                failures.push(format!("suggestions for {}: {error}", summary.id));
                Vec::new()
            }
        };
        docs.push(doc_card(&document, &suggestions));
    }
    if docs.is_empty() && !failures.is_empty() {
        bail!("could not load any document: {}", failures.join("; "));
    }
    Ok(docs)
}

// ---------------------------------------------------------------------------
// The one local write
// ---------------------------------------------------------------------------

/// Apply one optimistic-revision learning mutation and return the outcome
/// sentence the TUI shows (`mutate_learning` in `crates/cli/src/tui.rs`).
pub async fn mutate_learning(
    id: &str,
    revision: u64,
    mutation: &LearningMutation,
) -> anyhow::Result<String> {
    let id: LearningId = id.parse().context("invalid learning id")?;
    let pool = write_pool().await?;
    let result = mutate_learning_in(&pool, id, revision, mutation).await;
    pool.close().await;
    result
}

async fn mutate_learning_in(
    pool: &SqlitePool,
    id: LearningId,
    revision: u64,
    mutation: &LearningMutation,
) -> anyhow::Result<String> {
    let store = LearningStore::new();
    match mutation {
        LearningMutation::Activate => match store
            .activate(
                pool,
                id,
                revision,
                Verification::UserConfirmed { user: local_user() },
            )
            .await?
        {
            ActivationOutcome::Activated(_) => Ok("learning activated".to_owned()),
            ActivationOutcome::Conflict { .. } => {
                bail!("resolve the conflicting active learning first")
            }
        },
        LearningMutation::Reject => {
            store
                .reject(pool, id, revision, "rejected in learning journey")
                .await?;
            Ok("learning rejected".to_owned())
        }
        LearningMutation::SetPinned { pinned } => {
            store.set_pinned(pool, id, revision, *pinned).await?;
            Ok(if *pinned {
                "learning pinned"
            } else {
                "learning unpinned"
            }
            .to_owned())
        }
        LearningMutation::EditStatement { statement } => {
            let record = store.get(pool, id).await?.context("learning disappeared")?;
            let content = match record.content {
                LearningContent::Fact {
                    structured_value, ..
                } => LearningContent::Fact {
                    statement: statement.clone(),
                    structured_value,
                },
                LearningContent::Procedure(mut procedure) => {
                    procedure.summary = statement.clone();
                    LearningContent::Procedure(procedure)
                }
            };
            match store
                .edit(
                    pool,
                    id,
                    revision,
                    LearningPatch {
                        content: Some(content),
                        ..LearningPatch::default()
                    },
                )
                .await?
            {
                LearningMutationOutcome::Updated(_) => Ok("learning updated".to_owned()),
                LearningMutationOutcome::Duplicate { .. } => bail!("that learning already exists"),
                LearningMutationOutcome::Conflict { .. } => {
                    Ok("learning updated and returned to review".to_owned())
                }
            }
        }
        LearningMutation::Delete => {
            let deleted = store.delete(pool, id).await?;
            if deleted.id.is_some() {
                Ok("learning permanently deleted".to_owned())
            } else {
                bail!("learning was already deleted")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Card mapping — byte-for-byte the TUI harness's, so the two clients agree
// ---------------------------------------------------------------------------

fn skill_card(item: &RegistryItem) -> SkillCard {
    SkillCard {
        name: item.name.clone(),
        kind: registry_kind_label(item.kind).to_owned(),
        scope: scope_label(&item.scope),
        trust: trust_label(item.trust.tier).to_owned(),
        status: status_label(item.status).to_owned(),
        risk: risk_label(item.risk).to_owned(),
        description: item.description.clone(),
        permissions: item.permissions.iter().map(capability_verbatim).collect(),
    }
}

fn memory_card(record: &MemoryRecord) -> MemoryCard {
    let source = if record.provenance.is_empty() {
        "(no evidence)".to_owned()
    } else {
        record
            .provenance
            .iter()
            .map(evidence_source)
            .collect::<Vec<_>>()
            .join("; ")
    };
    MemoryCard {
        id: record.id.to_string(),
        statement: record.statement.clone(),
        class: memory_class_label(record.class).to_owned(),
        scope: scope_label(&record.scope),
        revision: record.valid_from.0.clone(),
        observed: record.observed_at.date_naive().to_string(),
        confidence: record.confidence,
        source,
    }
}

fn learning_card(record: &LearningRecord) -> LearningCard {
    let scope = match &record.scope {
        LearningScope::User(_) => "user".to_owned(),
        LearningScope::Repository(id) => format!(
            "repository {}",
            id.to_string().chars().take(8).collect::<String>()
        ),
        LearningScope::Provider(_) => "provider".to_owned(),
        LearningScope::Council(_) => "council".to_owned(),
    };
    let mut provenance = record
        .provenance
        .iter()
        .map(|source| match source {
            LearningProvenance::UserStatement { .. } => "user-confirmed",
            LearningProvenance::SuccessfulCommand { .. } => "locally verified",
            LearningProvenance::RepositoryObservation { .. } => "repository observation",
            LearningProvenance::AgentInference { .. } => "agent proposal",
            LearningProvenance::ToolOutput { .. } => "untrusted tool proposal",
            LearningProvenance::ExternalContent { .. } => "external proposal",
            LearningProvenance::CouncilResult { .. } => "council proposal",
        })
        .collect::<Vec<_>>();
    provenance.sort_unstable();
    provenance.dedup();
    LearningCard {
        id: record.id.to_string(),
        statement: record.content.summary().to_owned(),
        kind: match record.content.kind() {
            LearningKind::Fact => "fact".to_owned(),
            LearningKind::Procedure => "procedure".to_owned(),
        },
        state: match record.state {
            LearningState::Proposed => "proposed",
            LearningState::Active => "active",
            LearningState::Rejected => "rejected",
        }
        .to_owned(),
        scope,
        provenance: provenance.join(" + "),
        confidence: record.confidence,
        pinned: record.pinned,
        revision: record.revision,
    }
}

fn doc_card(document: &KnowledgeDocument, suggestions: &[Suggestion]) -> DocCard {
    DocCard {
        document_id: document.id.to_string(),
        title: document.title.clone(),
        scope: scope_label(&document.scope),
        status: document.status.as_str().to_owned(),
        mode: collab_mode_label(CollaborationMode::default_for_scope(&document.scope)).to_owned(),
        revision: format!("r{}", document.revision),
        blocks: document.blocks.iter().map(block_view).collect(),
        suggestions: suggestions.iter().map(suggestion_view).collect(),
    }
}

fn block_view(block: &DocumentBlock) -> DocBlockView {
    let (kind, text) = match &block.content {
        BlockContent::Heading { level, text } => (format!("heading h{level}"), text.clone()),
        BlockContent::Paragraph { text } => ("paragraph".to_owned(), text.clone()),
        BlockContent::Code { language, text } => (
            match language {
                Some(language) => format!("code {language}"),
                None => "code".to_owned(),
            },
            text.clone(),
        ),
        BlockContent::Diagram { format, .. } => {
            (format!("diagram {format}"), "(diagram)".to_owned())
        }
        BlockContent::Table { rows } => ("table".to_owned(), format!("({} rows)", rows.len())),
        BlockContent::Callout { kind, text } => (format!("callout {kind}"), text.clone()),
        BlockContent::Checklist { items } => {
            ("checklist".to_owned(), format!("({} items)", items.len()))
        }
        BlockContent::Query { query } => ("query".to_owned(), query.clone()),
        BlockContent::EmbeddedFile { path } => ("embed-file".to_owned(), path.clone()),
        BlockContent::EmbeddedSymbol { symbol } => ("embed-symbol".to_owned(), symbol.clone()),
        BlockContent::EmbeddedWorkflow { workflow } => {
            ("embed-workflow".to_owned(), workflow.clone())
        }
        BlockContent::EmbeddedSkill { skill } => ("embed-skill".to_owned(), skill.clone()),
    };
    DocBlockView {
        id: block.id.clone(),
        kind,
        text: text.replace('\n', " "),
        editable: block.primary_text().map(str::to_owned),
    }
}

fn suggestion_view(suggestion: &Suggestion) -> DocSuggestionView {
    DocSuggestionView {
        id: suggestion.id.clone(),
        block_id: suggestion.block_id.clone(),
        source_revision: suggestion.source_revision,
        status: suggestion_status_label(suggestion.status).to_owned(),
        author: document_author_label(&suggestion.author),
        range: format!("{}..{}", suggestion.range_start, suggestion.range_end),
        original: suggestion.original.clone(),
        replacement: suggestion.replacement.clone(),
        rationale: suggestion.rationale.clone(),
    }
}

fn capability_verbatim(capability: &CapabilityRequest) -> String {
    match capability {
        CapabilityRequest::FilesystemRead(value) => format!("filesystem_read: {value}"),
        CapabilityRequest::FilesystemWrite(value) => format!("filesystem_write: {value}"),
        CapabilityRequest::Command(value) => format!("command: {value}"),
        CapabilityRequest::Network(value) => format!("network: {value}"),
        CapabilityRequest::Secret(value) => format!("secret: {value}"),
    }
}

fn evidence_source(evidence: &EvidenceRef) -> String {
    match evidence {
        EvidenceRef::EventRange {
            session_id,
            from_sequence,
            to_sequence,
        } => format!("events {from_sequence}..{to_sequence} of session {session_id}"),
        EvidenceRef::Artifact {
            artifact,
            source_path,
        } => match source_path {
            Some(path) => format!("artifact {} ({path})", artifact.id),
            None => format!("artifact {}", artifact.id),
        },
        EvidenceRef::AgentAssertion {
            session_id,
            run_id,
            rationale,
        } => format!("asserted by run {run_id} (session {session_id}): {rationale}"),
    }
}

fn scope_label(scope: &Scope) -> String {
    match scope.key() {
        Some(key) => format!(
            "{} {}",
            scope.tier(),
            key.chars().take(8).collect::<String>()
        ),
        None => scope.tier().to_owned(),
    }
}

fn registry_kind_label(kind: RegistryItemKind) -> &'static str {
    match kind {
        RegistryItemKind::Tool => "tool",
        RegistryItemKind::Skill => "skill",
        RegistryItemKind::Plugin => "plugin",
        RegistryItemKind::Hook => "hook",
        RegistryItemKind::Command => "command",
    }
}

fn trust_label(tier: TrustTier) -> &'static str {
    match tier {
        TrustTier::Untrusted => "untrusted",
        TrustTier::Community => "community",
        TrustTier::Verified => "verified",
        TrustTier::FirstParty => "first-party",
    }
}

fn status_label(status: RegistryStatus) -> &'static str {
    match status {
        RegistryStatus::Draft => "draft",
        RegistryStatus::Active => "active",
        RegistryStatus::Modified => "modified",
        RegistryStatus::Deprecated => "deprecated",
    }
}

fn risk_label(risk: RiskClass) -> &'static str {
    match risk {
        RiskClass::Safe => "safe",
        RiskClass::Low => "low",
        RiskClass::Medium => "medium",
        RiskClass::High => "high",
    }
}

fn memory_class_label(class: MemoryClass) -> &'static str {
    match class {
        MemoryClass::Working => "working",
        MemoryClass::Episodic => "episodic",
        MemoryClass::Semantic => "semantic",
        MemoryClass::Procedural => "procedural",
        MemoryClass::Preference => "preference",
        MemoryClass::Failure => "failure",
        MemoryClass::Artifact => "artifact",
        MemoryClass::Code => "code",
    }
}

fn collab_mode_label(mode: CollaborationMode) -> &'static str {
    match mode {
        CollaborationMode::Ask => "ask",
        CollaborationMode::Suggest => "suggest",
        CollaborationMode::Edit => "edit",
        CollaborationMode::CoAuthor => "co-author",
        CollaborationMode::Review => "review",
        CollaborationMode::Maintain => "maintain",
    }
}

fn suggestion_status_label(status: SuggestionStatus) -> &'static str {
    match status {
        SuggestionStatus::Pending => "pending",
        SuggestionStatus::Accepted => "accepted",
        SuggestionStatus::Rejected => "rejected",
    }
}

fn document_author_label(author: &DocumentAuthor) -> String {
    match author {
        DocumentAuthor::Human { .. } => "human".to_owned(),
        DocumentAuthor::Agent { model, .. } => format!("agent ({model})"),
        DocumentAuthor::Integration { integration } => format!("integration ({integration})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The webview sends `crates/tui/src/action.rs`'s `LearningMutation` as an
    /// internally tagged object; every variant must round-trip, or a Pin
    /// button would fail with a serde message.
    #[test]
    fn learning_mutation_reads_the_webview_shape() {
        let cases = [
            (r#"{"type":"Activate"}"#, LearningMutation::Activate),
            (r#"{"type":"Reject"}"#, LearningMutation::Reject),
            (
                r#"{"type":"SetPinned","pinned":true}"#,
                LearningMutation::SetPinned { pinned: true },
            ),
            (
                r#"{"type":"EditStatement","statement":"use rg"}"#,
                LearningMutation::EditStatement {
                    statement: "use rg".to_owned(),
                },
            ),
            (r#"{"type":"Delete"}"#, LearningMutation::Delete),
        ];
        for (json, expected) in cases {
            let parsed: LearningMutation = serde_json::from_str(json).expect(json);
            assert_eq!(parsed, expected);
        }
    }

    /// The visible scopes are the TUI's: System always, the workspace when
    /// connected, and the repository — by its ANCHORED identity, so a
    /// selection made by subdirectory still sees the checkout's memories.
    #[test]
    fn scopes_follow_the_terminal_client() {
        let none = KnowledgeIdentity::default();
        assert_eq!(memory_scopes(&none), vec![Scope::System]);
        assert_eq!(
            learning_scopes(&none),
            vec![LearningScope::User(UserId("local".to_owned()))]
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = WorkspaceId::new();
        let both = KnowledgeIdentity {
            workspace: Some(workspace),
            repository: Some(dir.path().to_path_buf()),
        };
        let expected_repository = anchor_repository_id(dir.path());
        assert_eq!(
            memory_scopes(&both),
            vec![
                Scope::System,
                Scope::Workspace(workspace),
                Scope::Repository(expected_repository),
            ]
        );
        assert_eq!(
            learning_scopes(&both),
            vec![
                LearningScope::User(UserId("local".to_owned())),
                LearningScope::Repository(expected_repository),
            ]
        );
    }

    /// A read must not conjure a database: before the daemon has ever run,
    /// the answer is a sentence that says so, and no file appears.
    #[tokio::test]
    async fn a_missing_database_is_named_not_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(DATABASE_FILE);
        let error = read_pool_at(&path).await.expect_err("no database");
        let message = format!("{error:#}");
        assert!(message.contains("no knowledge database"), "{message}");
        assert!(message.contains("codypendentd creates it"), "{message}");
        assert!(!path.exists(), "the read-only path created a database");
    }

    /// Against a real, migrated database the four reads answer — empty, but
    /// as `loaded` answers, through the read-only opener the daemon's live
    /// file will be opened with.
    #[tokio::test]
    async fn reads_answer_against_a_migrated_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(DATABASE_FILE);
        let writer = knowledge_db::open(&path).await.expect("create and migrate");
        writer.close().await;

        let pool = read_pool_at(&path).await.expect("read-only open");
        let identity = KnowledgeIdentity {
            workspace: Some(WorkspaceId::new()),
            repository: Some(dir.path().to_path_buf()),
        };
        assert!(skills_in(&pool).await.expect("skills").is_empty());
        assert!(memories_in(&pool, &identity)
            .await
            .expect("memories")
            .is_empty());
        assert!(learnings_in(&pool, &identity)
            .await
            .expect("learnings")
            .is_empty());
        assert!(documents_in(&pool, &identity)
            .await
            .expect("documents")
            .is_empty());
        pool.close().await;
    }
}
