//! STEP 1.5 security tests, driven through the public policy API against real
//! directories and symlinks (Chapter 16 security list): path traversal, symlink
//! escape, deny precedence, lower-scope-cannot-widen, and unknown-key errors.

use std::os::unix::fs::symlink;
use std::path::Path;

use codypendent_daemon::policy::{Decision, EvalContext, PolicyEngine, ScopeVerdict};
use codypendent_protocol::ProposedAction;
use tempfile::tempdir;

/// A defaults engine plus a context that uses the worktree as both repository
/// and worktree, so the read scope (defaults = `$REPOSITORY`) and the write
/// scope (defaults = `$WORKTREE`) both resolve to the worktree — a single root
/// drives the path-canonicalization tests.
fn worktree_engine(worktree: &Path) -> (PolicyEngine, EvalContext) {
    let engine = PolicyEngine::with_defaults();
    let ctx = EvalContext::new(worktree, worktree);
    (engine, ctx)
}

#[test]
fn path_traversal_is_rejected() {
    let dir = tempdir().unwrap();
    let worktree = std::fs::canonicalize(dir.path()).unwrap();
    let (engine, ctx) = worktree_engine(&worktree);

    // `<worktree>/../../etc/passwd` canonicalizes outside the worktree.
    let escaping = worktree.join("../../etc/passwd");
    let scope = engine.file_read_scope(&ctx);
    assert_eq!(scope.classify(&escaping), ScopeVerdict::OutsideRoots);

    let decision = engine.evaluate(
        &ProposedAction::ReadFiles {
            paths: vec![escaping.to_string_lossy().into_owned()],
        },
        &ctx,
    );
    assert_eq!(decision.decision, Decision::Deny);
    assert_eq!(decision.reasons[0].code, "policy.path-out-of-scope");
}

#[test]
fn symlink_escape_is_rejected() {
    let dir = tempdir().unwrap();
    let worktree = std::fs::canonicalize(dir.path()).unwrap();
    let outside = tempdir().unwrap();
    let outside = std::fs::canonicalize(outside.path()).unwrap();
    std::fs::create_dir(worktree.join("wt")).unwrap();
    let worktree = worktree.join("wt");

    // A symlink *inside* the worktree that points to a directory outside it.
    let link = worktree.join("escape");
    symlink(&outside, &link).unwrap();

    let (engine, _) = worktree_engine(&worktree);
    let ctx = EvalContext::new(&worktree, &worktree);
    let write_scope = engine.file_write_scope(&ctx);

    // A write through the symlink resolves outside the worktree → not allowed.
    let through_link = link.join("stolen.txt");
    assert!(!write_scope.allows(&through_link));
    assert_eq!(
        write_scope.classify(&through_link),
        ScopeVerdict::OutsideRoots
    );

    // A genuine in-worktree path is still allowed.
    assert!(write_scope.allows(&worktree.join("ok.txt")));
}

#[test]
fn deny_list_wins_over_allowed_root() {
    let dir = tempdir().unwrap();
    let worktree = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(worktree.join(".git")).unwrap();
    std::fs::write(worktree.join(".git").join("config"), b"[core]").unwrap();

    // Defaults deny `$WORKTREE/.git` while the worktree itself is a write root.
    let engine = PolicyEngine::with_defaults();
    let ctx = EvalContext::new(&worktree, &worktree);
    let write_scope = engine.file_write_scope(&ctx);

    let git_config = worktree.join(".git").join("config");
    assert_eq!(write_scope.classify(&git_config), ScopeVerdict::Denied);
    assert!(!write_scope.allows(&git_config));

    // A sibling file under the same allowed root is fine.
    assert!(write_scope.allows(&worktree.join("main.rs")));
}

#[test]
fn lower_scope_cannot_widen_write_root() {
    let dir = tempdir().unwrap();
    let worktree = std::fs::canonicalize(dir.path()).unwrap();
    let elsewhere = tempdir().unwrap();
    let elsewhere = std::fs::canonicalize(elsewhere.path()).unwrap();

    // A repo policy that tries to ADD an out-of-scope write root beyond the
    // built-in `$WORKTREE`.
    let policy_dir = tempdir().unwrap();
    let policy_path = policy_dir.path().join("policy.toml");
    let toml = format!(
        "[filesystem]\nwrite = [\"$WORKTREE\", \"{}\"]\n",
        elsewhere.to_string_lossy()
    );
    std::fs::write(&policy_path, toml).unwrap();

    let engine = PolicyEngine::load(Some(&policy_path), None).expect("load policy");
    let ctx = EvalContext::new(&worktree, &worktree);
    let write_scope = engine.file_write_scope(&ctx);

    // The extra root was NOT granted (intersection, never union).
    assert!(!write_scope.allows(&elsewhere.join("evil.txt")));
    // The legitimate worktree root survives.
    assert!(write_scope.allows(&worktree.join("ok.txt")));
}

#[test]
fn unknown_key_fails_to_load() {
    let policy_dir = tempdir().unwrap();
    let policy_path = policy_dir.path().join("policy.toml");
    std::fs::write(
        &policy_path,
        "schema_version = 1\n[filesystem]\nbogus_key = true\n",
    )
    .unwrap();

    let result = PolicyEngine::load(Some(&policy_path), None);
    assert!(result.is_err(), "unknown key must be a load error");
}

#[test]
fn missing_file_is_skipped_not_an_error() {
    let policy_dir = tempdir().unwrap();
    let missing = policy_dir.path().join("does-not-exist.toml");
    let result = PolicyEngine::load(Some(&missing), None);
    assert!(result.is_ok(), "a missing path is skipped, not an error");
}
