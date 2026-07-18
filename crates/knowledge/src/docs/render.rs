//! Deterministic Markdown rendering and Git publication (STEP 4.4).
//!
//! Git is the **reviewed snapshot store**, not the collaboration algorithm. A
//! document revision renders to Markdown by a total, order-preserving function:
//! the same block list always renders byte-identical output (exit criterion 2,
//! "snapshot is reproducible"). Publication produces a [`PublishPlan`] — target,
//! changed files, and the resulting Git action — shown before approval, and, once
//! committed, records the `(document revision ↔ git commit)` pairing so staleness
//! (STEP 4.6) can compare a published document against the live graph.

use chrono::Utc;
use codypendent_protocol::DocumentId;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use super::model::{BlockContent, DocumentBlock, KnowledgeDocument};
use super::store::DocStoreError;

/// Render a document (title + blocks) to Markdown. Deterministic and total.
#[must_use]
pub fn render_document(title: &str, blocks: &[DocumentBlock]) -> String {
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(title);
    out.push('\n');
    for block in blocks {
        out.push('\n');
        render_block(&block.content, &mut out);
        out.push('\n');
    }
    out
}

/// Render one block. Stable per block kind; embed blocks keep their
/// `{{ kind:target }}` marker verbatim so the staleness engine (STEP 4.6) can
/// still resolve them in the published text.
fn render_block(content: &BlockContent, out: &mut String) {
    match content {
        BlockContent::Heading { level, text } => {
            let hashes = "#".repeat((*level).clamp(1, 6) as usize);
            out.push_str(&hashes);
            out.push(' ');
            out.push_str(text);
            out.push('\n');
        }
        BlockContent::Paragraph { text } => {
            out.push_str(text);
            out.push('\n');
        }
        BlockContent::Code { language, text } => {
            out.push_str("```");
            out.push_str(language.as_deref().unwrap_or(""));
            out.push('\n');
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n");
        }
        BlockContent::Diagram { format, source } => {
            out.push_str("```");
            out.push_str(format);
            out.push('\n');
            out.push_str(source);
            if !source.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n");
        }
        BlockContent::Table { rows } => render_table(rows, out),
        BlockContent::Callout { kind, text } => {
            // GitHub-style alert blockquote — deterministic, uppercased kind.
            out.push_str("> [!");
            out.push_str(&kind.to_uppercase());
            out.push_str("]\n> ");
            out.push_str(text);
            out.push('\n');
        }
        BlockContent::Checklist { items } => {
            for item in items {
                out.push_str(if item.checked { "- [x] " } else { "- [ ] " });
                out.push_str(&item.text);
                out.push('\n');
            }
        }
        BlockContent::Query { query } => {
            out.push_str("```query\n");
            out.push_str(query);
            out.push('\n');
            out.push_str("```\n");
        }
        BlockContent::EmbeddedFile { path } => {
            out.push('[');
            out.push_str(path);
            out.push_str("](");
            out.push_str(path);
            out.push_str(")\n");
        }
        BlockContent::EmbeddedSymbol { symbol } => {
            out.push_str("{{ symbol:");
            out.push_str(symbol);
            out.push_str(" }}\n");
        }
        BlockContent::EmbeddedWorkflow { workflow } => {
            out.push_str("{{ workflow:");
            out.push_str(workflow);
            out.push_str(" }}\n");
        }
        BlockContent::EmbeddedSkill { skill } => {
            out.push_str("{{ skill:");
            out.push_str(skill);
            out.push_str(" }}\n");
        }
    }
}

/// Render a table with the first row as the header (GitHub Markdown).
fn render_table(rows: &[Vec<String>], out: &mut String) {
    if rows.is_empty() {
        return;
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let write_row = |out: &mut String, row: &[String]| {
        out.push('|');
        for c in 0..columns {
            out.push(' ');
            out.push_str(row.get(c).map(String::as_str).unwrap_or(""));
            out.push_str(" |");
        }
        out.push('\n');
    };
    write_row(out, &rows[0]);
    out.push('|');
    for _ in 0..columns {
        out.push_str(" --- |");
    }
    out.push('\n');
    for row in &rows[1..] {
        write_row(out, row);
    }
}

/// Where a publication writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishTarget {
    /// Write the rendered Markdown to a repository file (via an approval-gated
    /// change set on the working tree).
    RepositoryFile { path: String },
    /// Commit the rendered Markdown to a dedicated docs branch.
    DocsBranchCommit { branch: String, path: String },
    /// Open a documentation pull request via the Phase 3 GitHub write path.
    DocumentationPr {
        branch: String,
        path: String,
        title: String,
    },
}

impl PublishTarget {
    /// The repo-relative file this target writes.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            PublishTarget::RepositoryFile { path }
            | PublishTarget::DocsBranchCommit { path, .. }
            | PublishTarget::DocumentationPr { path, .. } => path,
        }
    }

    /// A human description of the resulting Git action, shown before approval.
    #[must_use]
    pub fn git_action(&self) -> String {
        match self {
            PublishTarget::RepositoryFile { path } => {
                format!("write {path} in the working tree (approval-gated change set)")
            }
            PublishTarget::DocsBranchCommit { branch, path } => {
                format!("commit {path} on branch {branch}")
            }
            PublishTarget::DocumentationPr {
                branch,
                path,
                title,
            } => format!("open documentation PR \"{title}\" ({path} on {branch})"),
        }
    }
}

/// The plan a publish produces and shows before any approval: the target, the
/// files it changes, the Git action, and the exact rendered bytes (with their
/// hash, which is also what a published-snapshot row records).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishPlan {
    pub target: PublishTarget,
    pub changed_files: Vec<String>,
    pub git_action: String,
    pub rendered: String,
    pub rendered_hash: String,
    /// The document revision this plan renders.
    pub revision: u64,
}

/// Build the publication plan for `doc` to `target`. Pure — no side effects, no
/// approval; the caller displays it, gates it, then executes the Git action.
#[must_use]
pub fn plan_publication(doc: &KnowledgeDocument, target: PublishTarget) -> PublishPlan {
    let rendered = render_document(&doc.title, &doc.blocks);
    let rendered_hash = hex::encode(Sha256::digest(rendered.as_bytes()));
    let git_action = target.git_action();
    let changed_files = vec![target.path().to_string()];
    PublishPlan {
        target,
        changed_files,
        git_action,
        rendered,
        rendered_hash,
        revision: doc.revision,
    }
}

/// A recorded publication: which document revision was published, to where, at
/// which commit, and the hash of the rendered Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub id: String,
    pub document_id: DocumentId,
    pub revision: u64,
    pub target: String,
    pub git_commit: Option<String>,
    pub rendered_hash: String,
}

/// Record a completed publication (`plan` executed, producing `git_commit`),
/// storing the `(document revision ↔ git commit)` pairing staleness compares.
pub async fn record_publication(
    pool: &SqlitePool,
    document_id: DocumentId,
    plan: &PublishPlan,
    git_commit: Option<&str>,
) -> Result<Publication, DocStoreError> {
    let id = Uuid::now_v7().to_string();
    let target = plan.git_action.clone();
    sqlx::query(
        "INSERT INTO document_publications \
         (id, document_id, revision, target, git_commit, rendered_hash, published_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(document_id.to_string())
    .bind(plan.revision as i64)
    .bind(&target)
    .bind(git_commit)
    .bind(&plan.rendered_hash)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(Publication {
        id,
        document_id,
        revision: plan.revision,
        target,
        git_commit: git_commit.map(str::to_string),
        rendered_hash: plan.rendered_hash.clone(),
    })
}

/// The publication history for a document, newest first.
pub async fn publications(
    pool: &SqlitePool,
    document_id: DocumentId,
) -> Result<Vec<Publication>, DocStoreError> {
    let rows = sqlx::query(
        "SELECT id, revision, target, git_commit, rendered_hash FROM document_publications \
         WHERE document_id = ? ORDER BY published_at DESC, id DESC",
    )
    .bind(document_id.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| Publication {
            id: row.get("id"),
            document_id,
            revision: row.get::<i64, _>("revision") as u64,
            target: row.get("target"),
            git_commit: row.get("git_commit"),
            rendered_hash: row.get("rendered_hash"),
        })
        .collect())
}
