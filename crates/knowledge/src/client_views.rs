//! Shared display projection over the governed stores. Only the daemon serves
//! this data to clients; filesystem discovery and client-supplied scope keys
//! have no place at this boundary.
use crate::{
    anchor_repository_id, BlockContent, CapabilityRequest, CollaborationMode, DocumentAuthor,
    DocumentBlock, DocumentStore, EvidenceRef, KnowledgeDocument, LearningKind, LearningProvenance,
    LearningQuery, LearningRecord, LearningScope, LearningState, LearningStore, MemoryClass,
    MemoryRecord, MemoryStore, Registry, RegistryItem, RegistryItemKind, RegistryStatus, RiskClass,
    Scope, Suggestion, SuggestionStatus, SuggestionStore, TrustTier,
};
use anyhow::{bail, Context};
use codypendent_protocol::knowledge::*;
use codypendent_protocol::{SessionId, UserId, WorkspaceId};
use sqlx::SqlitePool;
use std::path::PathBuf;
#[derive(Debug, Clone, Default)]
pub struct KnowledgeIdentity {
    /// The workspace the current daemon connection carries, when connected.
    pub workspace: Option<WorkspaceId>,
    /// The checkout the operator selected on the Repository page, when any.
    pub repository: Option<PathBuf>,
}

fn memory_scopes(identity: &KnowledgeIdentity) -> Vec<Scope> {
    let mut scopes = vec![Scope::System, Scope::User(local_user())];
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

pub async fn skills_in(
    pool: &SqlitePool,
    identity: &KnowledgeIdentity,
) -> anyhow::Result<Vec<SkillCard>> {
    let items = Registry::new()
        .list(pool)
        .await
        .context("listing registry items")?;
    Ok(items
        .iter()
        .filter(|item| memory_scopes(identity).contains(&item.scope))
        .map(skill_card)
        .collect())
}

pub async fn memories_in(
    pool: &SqlitePool,
    identity: &KnowledgeIdentity,
) -> anyhow::Result<Vec<MemoryCard>> {
    let records = MemoryStore::new()
        .query(pool, &memory_scopes(identity), None)
        .await
        .context("querying memories")?;
    Ok(records.iter().map(memory_card).collect())
}

pub async fn learnings_in(
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

pub async fn documents_in(
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
    if !failures.is_empty() {
        bail!(
            "could not load all requested documents: {}",
            failures.join("; ")
        );
    }
    Ok(docs)
}

pub fn skill_card(item: &RegistryItem) -> SkillCard {
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

pub fn memory_card(record: &MemoryRecord) -> MemoryCard {
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

pub fn learning_card(record: &LearningRecord) -> LearningCard {
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

pub fn doc_card(document: &KnowledgeDocument, suggestions: &[Suggestion]) -> DocCard {
    DocCard {
        document_id: document.id,
        title: document.title.clone(),
        scope: scope_label(&document.scope),
        status: document.status.as_str().to_owned(),
        mode: collab_mode_label(CollaborationMode::default_for_scope(&document.scope)).to_owned(),
        revision: format!("r{}", document.revision),
        blocks: document.blocks.iter().map(block_view).collect(),
        suggestions: suggestions.iter().map(suggestion_view).collect(),
    }
}

pub fn block_view(block: &DocumentBlock) -> DocBlockView {
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

pub fn suggestion_view(suggestion: &Suggestion) -> DocSuggestionView {
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

pub fn evidence_source(evidence: &EvidenceRef) -> String {
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

/// Session ownership is checked by the socket server before invoking this.
/// Workspace/repository identities come from the durable session row.
pub async fn read_page(
    pool: &SqlitePool,
    session: SessionId,
    section: KnowledgeSection,
    offset: u32,
    limit: u32,
) -> anyhow::Result<KnowledgePage> {
    let (workspace, repository): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT workspace_id, repository FROM sessions WHERE id = ? AND tombstoned_at IS NULL",
    )
    .bind(session.to_string())
    .fetch_optional(pool)
    .await?
    .context("session not found")?;
    let identity = KnowledgeIdentity {
        workspace: workspace
            .map(|id| id.parse())
            .transpose()
            .context("invalid session workspace identity")?,
        repository: repository.map(PathBuf::from),
    };
    let limit = if limit == 0 { 50 } else { limit.min(100) } as usize;
    let mut page = KnowledgePage {
        section,
        skills: vec![],
        memories: vec![],
        learnings: vec![],
        documents: vec![],
        next_offset: None,
        warnings: vec![],
    };
    fn slice<T>(items: Vec<T>, offset: u32, limit: usize, next: &mut Option<u32>) -> Vec<T> {
        let mut items: Vec<T> = items
            .into_iter()
            .skip(offset as usize)
            .take(limit + 1)
            .collect();
        if items.len() > limit {
            items.pop();
            *next = offset.checked_add(limit as u32);
        }
        items
    }
    match section {
        KnowledgeSection::Skills => {
            page.skills = slice(
                skills_in(pool, &identity).await?,
                offset,
                limit,
                &mut page.next_offset,
            )
        }
        KnowledgeSection::Memories => {
            page.memories = slice(
                memories_in(pool, &identity).await?,
                offset,
                limit,
                &mut page.next_offset,
            )
        }
        KnowledgeSection::Learnings => {
            page.learnings = slice(
                learnings_in(pool, &identity).await?,
                offset,
                limit,
                &mut page.next_offset,
            )
        }
        KnowledgeSection::Documents => {
            page.documents = slice(
                documents_in(pool, &identity).await?,
                offset,
                limit,
                &mut page.next_offset,
            )
        }
        _ => bail!("unsupported knowledge section"),
    }
    if serde_json::to_vec(&page)?.len() > 8 * 1024 * 1024 {
        bail!("knowledge page exceeds 8 MiB; request fewer items");
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryClass, MemoryRecord, RetentionPolicy, Revision};
    use codypendent_protocol::{DataClassification, MemoryId};

    #[tokio::test]
    async fn knowledge_pages_derive_scopes_from_the_session_and_preserve_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        let pool = crate::db::open(&directory.path().join("store.db"))
            .await
            .unwrap();
        let session = SessionId::new();
        let now = chrono::Utc::now();
        sqlx::query("INSERT INTO sessions (id, workspace_id, title, state, created_at, updated_at, repository) VALUES (?, ?, 'knowledge', 'active', ?, ?, ?)")
            .bind(session.to_string()).bind(WorkspaceId::new().to_string()).bind(now.to_rfc3339()).bind(now.to_rfc3339())
            .bind(repository.to_string_lossy().to_string()).execute(&pool).await.unwrap();
        let record = |scope, statement: &str| MemoryRecord {
            id: MemoryId::new(),
            class: MemoryClass::Semantic,
            scope,
            statement: statement.to_owned(),
            structured_value: None,
            provenance: vec![],
            confidence: 0.7,
            observed_at: now,
            valid_from: Revision::sequence(1),
            valid_until: None,
            supersedes: vec![],
            sensitivity: DataClassification::Internal,
            retention: RetentionPolicy::default(),
        };
        let own = record(
            Scope::Repository(anchor_repository_id(&repository)),
            "This repository uses an isolated build directory.",
        );
        let system = record(
            Scope::System,
            "Review proposed changes before promoting a patch.",
        );
        let foreign = record(
            Scope::Repository(codypendent_protocol::RepositoryId::new()),
            "Another repository uses a separate convention.",
        );
        for item in [&own, &system, &foreign] {
            MemoryStore::new().insert(&pool, item).await.unwrap();
        }
        let first = read_page(&pool, session, KnowledgeSection::Memories, 0, 1)
            .await
            .unwrap();
        let second = read_page(
            &pool,
            session,
            KnowledgeSection::Memories,
            first.next_offset.unwrap(),
            1,
        )
        .await
        .unwrap();
        let cards: Vec<_> = first.memories.into_iter().chain(second.memories).collect();
        assert_eq!(
            cards
                .iter()
                .map(|card| card.id.as_str())
                .collect::<Vec<_>>(),
            vec![own.id.to_string(), system.id.to_string()]
        );
        assert!(second.next_offset.is_none());
        assert_eq!(
            (cards[0].confidence, cards[0].source.as_str()),
            (0.7, "(no evidence)")
        );
        assert!(read_page(&pool, session, KnowledgeSection::Unknown, 0, 1)
            .await
            .is_err());
        pool.close().await;
    }
}
