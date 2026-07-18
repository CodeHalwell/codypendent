//! The built-in tool registrations (Chapter 05, STEP 2.2).
//!
//! Phase 1 shipped five tools as plain code; Phase 2 registers them as governed
//! [`RegistryItem`]s so retrieval can rank them alongside skills and disclose
//! their permissions. The names and behaviours here mirror the runtime's real
//! tools (`crates/runtime/src/tools/`): `workspace.read_file`, `workspace.search`,
//! `shell.run`, `git.diff`, and `git.apply_patch`.
//!
//! Every built-in is [`System`](Scope::System)-scoped, [`BuiltIn`](Provenance::BuiltIn),
//! [`FirstParty`](TrustTier::FirstParty), and executable (built-ins ship no
//! scripts). Each one's [`RiskClass`] is derived from its declared permissions,
//! never set by hand, so the ranking penalty and the disclosure signal stay
//! consistent with skills.

use chrono::Utc;
use codypendent_protocol::RegistryItemId;
use sqlx::SqlitePool;

use crate::registry::{Registry, RegistryError};
use crate::types::{
    CapabilityRequest, Provenance, RegistryItem, RegistryItemKind, RegistryStatus, RiskClass,
    Scope, TrustMetadata, TrustTier, Version,
};

/// The version stamped on every built-in tool registration.
const BUILTIN_VERSION: &str = "1.0.0";

/// The scope root a built-in reads or writes within, expanded to the concrete
/// path by the policy engine at grant time.
const REPOSITORY_ROOT: &str = "$REPOSITORY";
/// The mutable worktree root the Git tools operate on.
const WORKTREE_ROOT: &str = "$WORKTREE";

/// The five Phase-1 tools as governed registry items.
///
/// Ids are freshly minted here; [`register_builtins`] reuses any existing id for
/// the same identity so a built-in's id is stable across restarts.
#[must_use]
pub fn builtin_tools() -> Vec<RegistryItem> {
    vec![
        tool(
            "workspace.read_file",
            "Return a line-numbered excerpt of a file, confined to the granted read scope.",
            &["read a file", "inspect source", "view file contents"],
            &["file", "read", "excerpt", "source", "view"],
            vec![CapabilityRequest::FilesystemRead(REPOSITORY_ROOT.into())],
        ),
        tool(
            "workspace.search",
            "Search the granted scope with ripgrep, returning typed file/line matches.",
            &[
                "search the codebase",
                "find a symbol",
                "grep for text",
                "locate a definition",
            ],
            &["search", "grep", "ripgrep", "find", "pattern"],
            vec![CapabilityRequest::FilesystemRead(REPOSITORY_ROOT.into())],
        ),
        tool(
            "shell.run",
            "Run an allow-listed program with a structured request in an empty environment, \
             spilling full output to the artifact store and returning a salient view.",
            &[
                "run a command",
                "run the tests",
                "build the project",
                "execute a program",
            ],
            &["shell", "command", "run", "execute", "cargo", "process"],
            // The concrete program set is the granted command allow-list; `*`
            // denotes "any allow-listed program" for the registry card.
            vec![CapabilityRequest::Command("*".into())],
        ),
        tool(
            "git.diff",
            "Produce the worktree's unstaged diff via `git diff`, spilling the full diff to the \
             artifact store. Read-only — it never mutates the worktree.",
            &[
                "show the diff",
                "inspect changes",
                "review the worktree diff",
            ],
            &["git", "diff", "changes", "worktree", "review"],
            vec![
                CapabilityRequest::FilesystemRead(WORKTREE_ROOT.into()),
                CapabilityRequest::Command("git".into()),
            ],
        ),
        tool(
            "git.apply_patch",
            "Apply a unified-diff patch to the worktree with `git apply`, running `git apply \
             --check` first and refusing (touching nothing) if the patch does not apply.",
            &["apply a patch", "modify files with a patch", "make an edit"],
            &["git", "patch", "apply", "edit", "change"],
            vec![
                CapabilityRequest::FilesystemWrite(WORKTREE_ROOT.into()),
                CapabilityRequest::Command("git".into()),
            ],
        ),
    ]
}

/// The built-in commands as governed registry items.
///
/// Phase 3 ships `/fix-ci` (STEP 3.2): investigate a failed GitHub check and
/// prepare a verified change set. Registering it as a [`RegistryItemKind::Command`]
/// item makes it discoverable in the Skill Studio alongside tools and skills; the
/// invocation (`/fix-ci`) drives the hard-coded repair workflow.
#[must_use]
pub fn builtin_commands() -> Vec<RegistryItem> {
    vec![
        command(
            "fix-ci",
            "Investigate a failed GitHub check and prepare a verified change set. \
             Invoked as `/fix-ci`; retrieves the check + logs, proposes a patch in an \
             isolated worktree, runs tests, and — on approval — updates the pull request.",
            &[
                "fix the failing CI",
                "repair a failed github check",
                "the ci is red",
                "make the checks pass",
            ],
            &["ci", "github", "check", "pull-request", "fix", "repair"],
            // `/fix-ci` runs git and reaches GitHub with the personal-mode token, so
            // its risk reflects both — the token bumps it to the highest tier.
            vec![
                CapabilityRequest::Command("git".into()),
                CapabilityRequest::Secret("github-token".into()),
            ],
        ),
        command(
            "update-docs",
            "Bring documentation in line with code changes (Phase 4 STEP 4.6). \
             Invoked as `/update-docs`; diffs documents' resolved `{{ symbol:… }}` \
             links against the live code graph and, for each stale reference, drafts \
             a Maintain-mode suggestion (never a direct edit) citing the causing \
             change for review.",
            &[
                "update the docs",
                "the documentation is stale",
                "docs reference an old symbol",
                "fix stale documentation",
            ],
            &["docs", "documentation", "staleness", "maintain", "symbol"],
            // Reads the repository to resolve symbols; proposes suggestions on
            // documents (no direct writes, no network).
            vec![CapabilityRequest::FilesystemRead(REPOSITORY_ROOT.into())],
        ),
    ]
}

/// Register (or refresh) every built-in tool and command.
///
/// Each is upserted at [`Scope::System`]; an existing registration of the same
/// identity has its `id`/`created_at` reused so ids stay stable across restarts.
/// Each upsert appends a `RegistryItemChanged` outbox row (via [`Registry::upsert`]).
pub async fn register_builtins(pool: &SqlitePool) -> Result<(), RegistryError> {
    let registry = Registry::new();
    for mut item in builtin_tools().into_iter().chain(builtin_commands()) {
        if let Some(existing) = registry
            .by_identity(pool, item.kind, &item.name, &item.scope)
            .await?
        {
            item.id = existing.id;
            item.created_at = existing.created_at;
        }
        registry.upsert(pool, &item).await?;
    }
    Ok(())
}

/// Build one built-in command [`RegistryItem`] (kind [`RegistryItemKind::Command`]).
/// Shares [`tool`]'s shape but is invoked, not called by the model as a tool.
fn command(
    name: &str,
    description: &str,
    intents: &[&str],
    keywords: &[&str],
    permissions: Vec<CapabilityRequest>,
) -> RegistryItem {
    RegistryItem {
        kind: RegistryItemKind::Command,
        ..tool(name, description, intents, keywords, permissions)
    }
}

/// Build one built-in tool [`RegistryItem`] with risk derived from its
/// permissions.
fn tool(
    name: &str,
    description: &str,
    intents: &[&str],
    keywords: &[&str],
    permissions: Vec<CapabilityRequest>,
) -> RegistryItem {
    let risk = RiskClass::from_permissions(&permissions);
    let now = Utc::now();
    RegistryItem {
        id: RegistryItemId::new(),
        kind: RegistryItemKind::Tool,
        name: name.to_string(),
        version: Version(BUILTIN_VERSION.to_string()),
        scope: Scope::System,
        description: description.to_string(),
        intents: intents.iter().map(|s| s.to_string()).collect(),
        keywords: keywords.iter().map(|s| s.to_string()).collect(),
        examples: Vec::new(),
        input_schema: None,
        output_schema: None,
        dependencies: Vec::new(),
        permissions,
        risk,
        provenance: Provenance::BuiltIn,
        trust: TrustMetadata {
            publisher: "codypendent".to_string(),
            signature_required: false,
            signature: None,
            tier: TrustTier::FirstParty,
        },
        status: RegistryStatus::Active,
        // Built-ins ship no scripts, so they are always runnable.
        content_hash: String::new(),
        executable: true,
        created_at: now,
        updated_at: now,
    }
}
