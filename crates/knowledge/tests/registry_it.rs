//! STEP 2.2: the scoped registry + skill-package loader.
//!
//! Covers package parse round-trip (the shipped `fix-ci` reference skill),
//! unknown-key rejection, content-hash change detection, and scope shadowing
//! (the same skill id registered at two scopes — both rows remain visible, the
//! more specific one wins selection).

use std::path::{Path, PathBuf};

use codypendent_knowledge::manifest::ManifestError;
use codypendent_knowledge::{
    db, load_package, register_builtins, resolve_shadowed, Provenance, Registry, RegistryError,
    RegistryItemKind, RegistryStatus, RiskClass, Scope, TrustTier, Version,
};
use codypendent_protocol::{RepositoryId, UserId, WorkspaceId};

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

/// Absolute path to the shipped reference skill package.
fn fix_ci_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/skills/fix-ci")
}

/// Write a minimal skill package (`skill.toml` + `SKILL.md`) into `dir`.
fn write_package(dir: &Path, id: &str, scope: &str, version: &str, status: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let manifest = format!(
        "schema_version = 1\n\
         id = \"{id}\"\n\
         name = \"Temp Skill\"\n\
         version = \"{version}\"\n\
         scope = \"{scope}\"\n\
         status = \"{status}\"\n\
         description = \"A temporary skill for tests.\"\n\
         intents = [\"temp\"]\n\
         languages = [\"rust\"]\n\
         \n\
         [permissions]\n\
         filesystem_read = [\"$REPOSITORY\"]\n\
         \n\
         [entrypoints]\n\
         instructions = \"SKILL.md\"\n\
         \n\
         [trust]\n\
         publisher = \"local-user\"\n\
         signature_required = false\n"
    );
    std::fs::write(dir.join("skill.toml"), manifest).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
}

#[test]
fn package_parse_round_trip() {
    // The manifest declares scope = "repository", so it must be registered under a
    // Repository scope.
    let item = load_package(&fix_ci_dir(), Scope::Repository(RepositoryId::new())).unwrap();

    assert_eq!(item.kind, RegistryItemKind::Skill);
    // `name` is the stable id slug, not the human display title.
    assert_eq!(item.name, "rust.fix-ci");
    assert_eq!(item.version, Version("0.1.0".to_string()));
    assert_eq!(
        item.description,
        "Diagnose and repair Rust GitHub Actions failures."
    );
    assert_eq!(
        item.intents,
        vec!["ci failure", "rust tests", "github actions", "clippy"]
    );
    assert_eq!(item.status, RegistryStatus::Draft);

    // [permissions] flattens to 1 read + 1 write + 2 commands + 1 network = 5.
    assert_eq!(item.permissions.len(), 5);
    // Writes/commands/network → Medium; no secrets → not High.
    assert_eq!(item.risk, RiskClass::Medium);

    // required_tools (3) + optional_tools (3).
    assert_eq!(item.dependencies.len(), 6);
    assert_eq!(item.dependencies.iter().filter(|d| !d.optional).count(), 3);

    // local-user publisher → first-party trust.
    assert_eq!(item.trust.publisher, "local-user");
    assert_eq!(item.trust.tier, TrustTier::FirstParty);

    // A non-empty scripts/ entrypoint makes the skill non-executable in Phase 2.
    assert!(!item.executable);

    // Languages plus the human title are retained as keywords.
    assert!(item.keywords.iter().any(|k| k == "rust"));

    assert!(!item.content_hash.is_empty());
    assert!(matches!(item.provenance, Provenance::Package { .. }));
}

#[test]
fn unknown_top_level_key_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_package(
        dir,
        "temp.skill",
        "repository",
        "0.1.0",
        "active",
        "# Temp\n",
    );
    // Append a bogus top-level key that `deny_unknown_fields` must reject.
    let mut manifest = std::fs::read_to_string(dir.join("skill.toml")).unwrap();
    manifest.push_str("\nbogus_key = \"nope\"\n");
    std::fs::write(dir.join("skill.toml"), manifest).unwrap();

    let err = load_package(dir, Scope::Repository(RepositoryId::new())).unwrap_err();
    assert!(matches!(err, ManifestError::Toml(_)), "got {err:?}");
}

#[test]
fn missing_entrypoint_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_package(
        dir,
        "temp.skill",
        "repository",
        "0.1.0",
        "active",
        "# Temp\n",
    );
    // Declare a references/ entrypoint that does not exist on disk.
    let mut manifest = std::fs::read_to_string(dir.join("skill.toml")).unwrap();
    manifest = manifest.replace(
        "instructions = \"SKILL.md\"",
        "instructions = \"SKILL.md\"\nreferences = \"references/\"",
    );
    std::fs::write(dir.join("skill.toml"), manifest).unwrap();

    let err = load_package(dir, Scope::Repository(RepositoryId::new())).unwrap_err();
    assert!(
        matches!(err, ManifestError::MissingEntrypoint { ref path } if path == "references/"),
        "got {err:?}"
    );
}

#[test]
fn scope_mismatch_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_package(
        dir,
        "temp.skill",
        "repository",
        "0.1.0",
        "active",
        "# Temp\n",
    );
    // Manifest says "repository"; register under a User scope.
    let err = load_package(dir, Scope::User(UserId("u".into()))).unwrap_err();
    assert!(
        matches!(err, ManifestError::ScopeMismatch { ref declared, ref expected }
            if declared == "repository" && expected == "user"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn hash_change_flags_modified_at_same_version() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();

    let pkg = tempfile::tempdir().unwrap();
    let dir = pkg.path();
    let scope = Scope::Repository(RepositoryId::new());

    write_package(
        dir,
        "temp.skill",
        "repository",
        "0.1.0",
        "active",
        "# One\n",
    );
    let first = registry
        .register_package(&pool, dir, scope.clone())
        .await
        .unwrap();
    assert_eq!(first.status, RegistryStatus::Active);

    // Re-register unchanged: not modified, id preserved.
    let unchanged = registry
        .register_package(&pool, dir, scope.clone())
        .await
        .unwrap();
    assert_eq!(unchanged.status, RegistryStatus::Active);
    assert_eq!(
        unchanged.id, first.id,
        "id is stable across re-registration"
    );

    // Change a package file, keep the version → flagged Modified.
    std::fs::write(
        dir.join("SKILL.md"),
        "# Two — edited without a version bump\n",
    )
    .unwrap();
    let modified = registry
        .register_package(&pool, dir, scope.clone())
        .await
        .unwrap();
    assert_eq!(modified.version, Version("0.1.0".to_string()));
    assert_ne!(modified.content_hash, first.content_hash);
    assert_eq!(modified.status, RegistryStatus::Modified);
    assert_eq!(modified.id, first.id);

    // Still one row for this identity.
    let all = registry.list(&pool).await.unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn scope_shadowing_keeps_both_rows_and_resolves_the_specific_one() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();

    // Same id at two scopes → two distinct rows.
    let user_pkg = tempfile::tempdir().unwrap();
    write_package(
        user_pkg.path(),
        "rust.fix-ci",
        "user",
        "0.1.0",
        "active",
        "# user\n",
    );
    let user_item = registry
        .register_package(&pool, user_pkg.path(), Scope::User(UserId("u".into())))
        .await
        .unwrap();

    let ws_pkg = tempfile::tempdir().unwrap();
    write_package(
        ws_pkg.path(),
        "rust.fix-ci",
        "workspace",
        "0.1.0",
        "active",
        "# ws\n",
    );
    let ws_item = registry
        .register_package(&pool, ws_pkg.path(), Scope::Workspace(WorkspaceId::new()))
        .await
        .unwrap();

    // Both rows are visible.
    let all = registry.list(&pool).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_ne!(user_item.id, ws_item.id);

    // by_identity distinguishes them by scope.
    assert!(registry
        .by_identity(
            &pool,
            RegistryItemKind::Skill,
            "rust.fix-ci",
            &Scope::User(UserId("u".into()))
        )
        .await
        .unwrap()
        .is_some());

    // Selection: the workspace (more specific) row shadows the user row.
    let winner = resolve_shadowed(&all).unwrap();
    assert_eq!(winner.scope, ws_item.scope);
    assert_eq!(winner.id, ws_item.id);
}

#[tokio::test]
async fn register_builtins_registers_the_phase1_tools() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();

    register_builtins(&pool).await.unwrap();
    let items = registry.list(&pool).await.unwrap();

    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    for expected in [
        "workspace.read_file",
        "workspace.search",
        "shell.run",
        "git.diff",
        "git.apply_patch",
    ] {
        assert!(names.contains(&expected), "missing built-in {expected}");
    }
    assert!(items.iter().all(|i| i.kind == RegistryItemKind::Tool));
    assert!(items.iter().all(|i| i.scope == Scope::System));
    assert!(items.iter().all(|i| i.trust.tier == TrustTier::FirstParty));
    assert!(items.iter().all(|i| i.executable));

    // Idempotent: re-registering keeps ids stable and does not duplicate rows.
    let shell_before = registry
        .by_identity(&pool, RegistryItemKind::Tool, "shell.run", &Scope::System)
        .await
        .unwrap()
        .unwrap();
    register_builtins(&pool).await.unwrap();
    let shell_after = registry
        .by_identity(&pool, RegistryItemKind::Tool, "shell.run", &Scope::System)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shell_before.id, shell_after.id);
    assert_eq!(registry.list(&pool).await.unwrap().len(), 5);
}

#[tokio::test]
async fn upsert_and_remove_write_outbox_events() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();

    let pkg = tempfile::tempdir().unwrap();
    write_package(
        pkg.path(),
        "temp.skill",
        "repository",
        "0.1.0",
        "active",
        "# temp\n",
    );
    let item = registry
        .register_package(&pool, pkg.path(), Scope::Repository(RepositoryId::new()))
        .await
        .unwrap();

    // The upsert enqueued one outbox row.
    let after_upsert = codypendent_knowledge::outbox::unprocessed(&pool, 10)
        .await
        .unwrap();
    assert_eq!(after_upsert.len(), 1);
    assert_eq!(after_upsert[0].event_kind, "registry_item_changed");
    assert_eq!(after_upsert[0].entity_id, item.id.to_string());

    // Remove also enqueues an event; get() no longer finds it.
    assert!(registry.remove(&pool, item.id).await.unwrap());
    assert!(registry.get(&pool, item.id).await.unwrap().is_none());
    let after_remove = codypendent_knowledge::outbox::unprocessed(&pool, 10)
        .await
        .unwrap();
    assert_eq!(after_remove.len(), 2);
}

/// A wrapped `RegistryError::Manifest` surfaces the load failure through the
/// registry API too.
#[tokio::test]
async fn register_package_propagates_manifest_errors() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    let pkg = tempfile::tempdir().unwrap();
    write_package(
        pkg.path(),
        "temp.skill",
        "repository",
        "9.9.9",
        "active",
        "# t\n",
    );
    // Wrong registration scope → ScopeMismatch, wrapped as RegistryError::Manifest.
    let err = registry
        .register_package(&pool, pkg.path(), Scope::User(UserId("u".into())))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RegistryError::Manifest(ManifestError::ScopeMismatch { .. })
        ),
        "got {err:?}"
    );
}
