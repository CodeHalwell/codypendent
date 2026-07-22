//! Policy engine and capability grants (STEP 1.5).
//!
//! The engine turns a [`ProposedAction`] into a [`PolicyDecision`]:
//! `Allow`, `Deny`, or `RequireApproval`, together with the machine-readable
//! reasons, an optional minted [`CapabilityGrant`], and the [`PolicyVersion`]
//! the decision was made under. It is the single gate every model- or
//! user-proposed side effect passes through
//! ([Chapter 11](../../../docs/docs/11-security-and-governance.md)); a proposal
//! is denied on policy alone regardless of what a model says.
//!
//! Layering and the merge invariant live in [`config`]; scopes, capabilities,
//! and path canonicalization live in [`scope`]. This module wires them together
//! and expands `$REPOSITORY`/`$WORKTREE`/`$HOME` per evaluation against an
//! [`EvalContext`]. An [`EvalContext`] also carries a [`ModeOverlay`] so a
//! caller (the STEP 1.10 agent loop) can layer an `AgentMode`'s restrictions —
//! e.g. `Explore` denies writes — on top of the file policy without this module
//! owning the mode bundles.

mod config;
mod scope;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use codypendent_protocol::ProposedAction;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use config::{ApprovalAction, PolicyLoadError};
pub use scope::{Capability, CommandScope, NetworkDefault, NetworkScope, PathScope, ScopeVerdict};

use config::MergedPolicy;

/// How long a minted capability grant remains valid. Capabilities are
/// invocation-scoped and time-limited (Chapter 11).
const CAPABILITY_GRANT_TTL_MINUTES: i64 = 15;

/// The `host:port` a GitHub mutation must be network-authorized against. GitHub
/// writes are network-scoped to exactly this endpoint (Phase 3 STEP 3.1).
pub const GITHUB_API_ENDPOINT: &str = "api.github.com:443";

/// The three possible dispositions of a policy evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Decision {
    /// Permit immediately.
    Allow,
    /// Refuse; no capability is granted.
    Deny,
    /// Permit only once a human approves; a grant is minted but gated.
    RequireApproval,
}

/// A machine-readable justification attached to a decision. `code` is a stable
/// dotted identifier (e.g. `policy.path-out-of-scope`); `message` is for humans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyReason {
    pub code: String,
    pub message: String,
}

impl PolicyReason {
    /// Build a reason from a stable code and a human message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// A capability minted for a decision, valid until `expires_at`. For a
/// `RequireApproval` decision the grant exists but must not be used until the
/// approval is resolved; for `Deny` there is no grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityGrant {
    pub capability: Capability,
    pub expires_at: DateTime<Utc>,
}

/// A stable identifier for the merged policy a decision was made under: the
/// hex SHA-256 of the merged policy's canonical serialization. Identical merged
/// policies yield identical versions; any change to the effective policy
/// changes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PolicyVersion(pub String);

impl std::fmt::Display for PolicyVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The outcome of evaluating a [`ProposedAction`] (Chapter 14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub decision: Decision,
    pub reasons: Vec<PolicyReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_grant: Option<CapabilityGrant>,
    pub policy_version: PolicyVersion,
}

/// A mode-derived restriction layered on top of the file policy. Modes
/// (`Ask`/`Explore`/`Plan`/`Build`/`Review`) are enforced in policy, not just
/// prompts. This module does not own the `AgentMode → bundle` mapping (that is
/// STEP 1.10); a caller sets these switches and the engine honors them by
/// *further* denying — an overlay can never grant what the file policy forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeOverlay {
    /// Whether the mode permits filesystem writes and repository mutations.
    pub write_allowed: bool,
    /// Whether the mode permits command execution.
    pub command_allowed: bool,
    /// Whether the mode permits network connections.
    pub network_allowed: bool,
}

impl ModeOverlay {
    /// No mode restriction: the file policy alone decides.
    pub fn permissive() -> Self {
        Self {
            write_allowed: true,
            command_allowed: true,
            network_allowed: true,
        }
    }

    /// A read-only overlay: writes, commands, and network are all denied
    /// (a convenient starting point for `Ask`/`Explore`).
    pub fn read_only() -> Self {
        Self {
            write_allowed: false,
            command_allowed: false,
            network_allowed: false,
        }
    }
}

impl Default for ModeOverlay {
    fn default() -> Self {
        Self::permissive()
    }
}

/// Per-evaluation context: the repository and worktree roots that
/// `$REPOSITORY`/`$WORKTREE` expand to, plus the [`ModeOverlay`] in force.
#[derive(Debug, Clone)]
pub struct EvalContext {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub mode: ModeOverlay,
}

impl EvalContext {
    /// Context for a repository/worktree with no mode restriction.
    pub fn new(repository: impl Into<PathBuf>, worktree: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
            worktree: worktree.into(),
            mode: ModeOverlay::permissive(),
        }
    }

    /// Set the mode overlay.
    pub fn with_mode(mut self, mode: ModeOverlay) -> Self {
        self.mode = mode;
        self
    }
}

/// Evaluates proposed actions against a merged policy.
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    merged: MergedPolicy,
    version: PolicyVersion,
}

impl PolicyEngine {
    /// An engine over the built-in defaults (no policy files).
    pub fn with_defaults() -> Self {
        Self::from_merged(MergedPolicy::builtin_defaults())
    }

    /// The built-in defaults, additionally admitting `endpoints` on the network
    /// allow-list. GitHub mutations are network-scoped to [`GITHUB_API_ENDPOINT`],
    /// so the daemon uses this (rather than [`with_defaults`]) when a GitHub
    /// client is configured: the endpoint must be reachable for a mutation to
    /// reach the approval gate at all, but admitting it grants nothing on its own
    /// — every GitHub write still returns `RequireApproval`.
    ///
    /// [`with_defaults`]: PolicyEngine::with_defaults
    pub fn with_defaults_allowing_network(endpoints: impl IntoIterator<Item = String>) -> Self {
        let mut merged = MergedPolicy::builtin_defaults();
        merged.network_allow.extend(endpoints);
        Self::from_merged(merged)
    }

    /// Load and merge policy from explicit file paths over the built-in
    /// defaults: `config_policy` (User layer) then `repo_policy` (Repository
    /// layer, narrowest). A `None` path — or a path that does not exist — is
    /// skipped. A malformed file (including an unknown key) is an error.
    ///
    /// Passing explicit paths keeps the engine testable without reading real
    /// user directories.
    pub fn load(
        repo_policy: Option<&Path>,
        config_policy: Option<&Path>,
    ) -> Result<Self, PolicyLoadError> {
        let mut merged = MergedPolicy::builtin_defaults();
        if let Some(path) = config_policy {
            if let Some(raw) = config::load_layer(path)? {
                merged.apply_overlay(&raw);
            }
        }
        if let Some(path) = repo_policy {
            if let Some(raw) = config::load_layer(path)? {
                merged.apply_overlay(&raw);
            }
        }
        Ok(Self::from_merged(merged))
    }

    fn from_merged(merged: MergedPolicy) -> Self {
        let version = version_of(&merged);
        Self { merged, version }
    }

    /// The version identifying this engine's merged policy.
    pub fn policy_version(&self) -> &PolicyVersion {
        &self.version
    }

    /// The read scope for `ctx`, with `$REPOSITORY`/`$WORKTREE`/`$HOME`
    /// expanded and every root canonicalized. Exposed so the tool layer can
    /// check specific paths under a granted capability.
    pub fn file_read_scope(&self, ctx: &EvalContext) -> PathScope {
        build_path_scope(&self.merged.fs_read, &self.merged.fs_deny, ctx)
    }

    /// The write scope for `ctx` (see [`file_read_scope`]).
    ///
    /// [`file_read_scope`]: PolicyEngine::file_read_scope
    pub fn file_write_scope(&self, ctx: &EvalContext) -> PathScope {
        build_path_scope(&self.merged.fs_write, &self.merged.fs_deny, ctx)
    }

    /// The command scope (allow-list plus the wall-clock ceiling).
    pub fn command_scope(&self) -> CommandScope {
        CommandScope {
            allowed_programs: self.merged.shell_allowed_programs.clone(),
            maximum_seconds: self.merged.shell_maximum_seconds,
        }
    }

    /// The network scope (allow-list plus the default disposition).
    pub fn network_scope(&self) -> NetworkScope {
        NetworkScope {
            allow: self.merged.network_allow.clone(),
            default: self.merged.network_default,
        }
    }

    /// Evaluate a proposed action, returning the decision, its reasons, any
    /// minted capability grant, and the policy version.
    pub fn evaluate(&self, action: &ProposedAction, ctx: &EvalContext) -> PolicyDecision {
        match action {
            ProposedAction::ReadFiles { paths } => self.eval_read(paths, ctx),
            ProposedAction::WritePatch { .. } => self.eval_write(ctx),
            ProposedAction::ExecuteCommand { program, .. } => self.eval_command(program, ctx),
            ProposedAction::NetworkRequest { destination } => self.eval_network(destination, ctx),
            ProposedAction::GitCommit { .. } => self.eval_git(GitOp::Commit, ctx),
            ProposedAction::GitPush { .. } => self.eval_git(GitOp::Push, ctx),
            ProposedAction::GitHubMutation { .. } => self.eval_github_mutation(ctx),
            ProposedAction::BlackboardPost { .. } | ProposedAction::BlackboardQuery { .. } => {
                self.eval_blackboard()
            }
            _ => self.deny(PolicyReason::new(
                "policy.unsupported-action",
                "the proposed action is not recognized by this policy engine",
            )),
        }
    }

    /// A blackboard post/query (Phase 5 STEP 5.3) is always permitted: it targets
    /// only the workflow run's OWN typed-artifact channel — not the filesystem, the
    /// repository, or any remote — and the `blackboard.*` tools are offered solely
    /// inside a workflow node's agent run. It grants no capability (the tool needs
    /// no path/command/network scope) and is recorded purely so the board access is
    /// traced like any other tool call. Writes that DO escape the run (files, git,
    /// GitHub) keep their existing approval gates; this does not widen them.
    fn eval_blackboard(&self) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![PolicyReason::new(
                "policy.blackboard-allowed",
                "a workflow blackboard access targets only the run's own artifact channel",
            )],
            capability_grant: None,
            policy_version: self.version.clone(),
        }
    }

    fn eval_read(&self, paths: &[String], ctx: &EvalContext) -> PolicyDecision {
        let scope = self.file_read_scope(ctx);
        let mut denied: Vec<&str> = Vec::new();
        let mut outside: Vec<&str> = Vec::new();
        for path in paths {
            match scope.classify(Path::new(path)) {
                ScopeVerdict::Allowed => {}
                ScopeVerdict::Denied => denied.push(path),
                ScopeVerdict::OutsideRoots => outside.push(path),
            }
        }
        if !denied.is_empty() {
            return self.deny(PolicyReason::new(
                "policy.path-denied",
                format!("read blocked by the deny list: {}", denied.join(", ")),
            ));
        }
        if !outside.is_empty() {
            return self.deny(PolicyReason::new(
                "policy.path-out-of-scope",
                format!("read outside the allowed roots: {}", outside.join(", ")),
            ));
        }
        self.allow(
            Capability::FileRead(scope),
            PolicyReason::new("policy.read-allowed", "all paths are within the read scope"),
        )
    }

    fn eval_write(&self, ctx: &EvalContext) -> PolicyDecision {
        if !ctx.mode.write_allowed {
            return self.deny(PolicyReason::new(
                "policy.write-denied-by-mode",
                "the active mode forbids filesystem writes",
            ));
        }
        let scope = self.file_write_scope(ctx);
        if scope.roots.is_empty() {
            return self.deny(PolicyReason::new(
                "policy.no-write-scope",
                "no writable roots are in scope",
            ));
        }
        self.allow(
            Capability::FileWrite(scope),
            PolicyReason::new(
                "policy.write-allowed",
                "writes are permitted within the worktree scope",
            ),
        )
    }

    fn eval_command(&self, program: &str, ctx: &EvalContext) -> PolicyDecision {
        if !ctx.mode.command_allowed {
            return self.deny(PolicyReason::new(
                "policy.command-denied-by-mode",
                "the active mode forbids command execution",
            ));
        }
        let scope = self.command_scope();
        if !scope.allows_program(program) {
            return self.deny(PolicyReason::new(
                "policy.program-not-allowlisted",
                format!("`{program}` is not in the shell allow-list"),
            ));
        }
        // The built-in default requires approval for every allow-listed command.
        self.require(
            Capability::CommandExecute(scope),
            PolicyReason::new(
                "policy.command-requires-approval",
                format!("`{program}` is allow-listed; shell execution requires approval"),
            ),
        )
    }

    fn eval_network(&self, destination: &str, ctx: &EvalContext) -> PolicyDecision {
        if !ctx.mode.network_allowed {
            return self.deny(PolicyReason::new(
                "policy.network-denied-by-mode",
                "the active mode forbids network connections",
            ));
        }
        let scope = self.network_scope();
        if scope.allows(destination) {
            return self.allow(
                Capability::NetworkConnect(scope),
                PolicyReason::new(
                    "policy.network-allowed",
                    format!("`{destination}` is permitted by the network policy"),
                ),
            );
        }
        self.deny(PolicyReason::new(
            "policy.network-denied",
            format!("`{destination}` is not permitted by the network policy"),
        ))
    }

    /// Evaluate a remote GitHub write (Phase 3 STEP 3.1). A GitHub mutation is a
    /// network write to the GitHub API endpoint: it is denied unless the active
    /// mode permits network access and the network policy admits
    /// [`GITHUB_API_ENDPOINT`], and it *always* requires approval — every remote
    /// write is approval-gated (Chapter 10). The minted grant is a
    /// `NetworkConnect` capability scoped to the GitHub endpoint.
    fn eval_github_mutation(&self, ctx: &EvalContext) -> PolicyDecision {
        if !ctx.mode.network_allowed {
            return self.deny(PolicyReason::new(
                "policy.github-denied-by-mode",
                "the active mode forbids network connections",
            ));
        }
        let scope = self.network_scope();
        if !scope.allows(GITHUB_API_ENDPOINT) {
            return self.deny(PolicyReason::new(
                "policy.github-network-denied",
                format!("`{GITHUB_API_ENDPOINT}` is not permitted by the network policy"),
            ));
        }
        self.require(
            Capability::NetworkConnect(scope),
            PolicyReason::new(
                "policy.github-requires-approval",
                "GitHub writes require approval",
            ),
        )
    }

    fn eval_git(&self, op: GitOp, ctx: &EvalContext) -> PolicyDecision {
        if !ctx.mode.write_allowed {
            return self.deny(PolicyReason::new(
                "policy.git-denied-by-mode",
                "the active mode forbids repository mutations",
            ));
        }
        let (action, capability, name) = match op {
            GitOp::Commit => (self.merged.git_commit, Capability::GitCommit, "commit"),
            GitOp::Push => (self.merged.git_push, Capability::GitPush, "push"),
        };
        match action {
            ApprovalAction::Allow => self.allow(
                capability,
                PolicyReason::new(
                    "policy.git-allowed",
                    format!("git {name} is permitted by policy"),
                ),
            ),
            ApprovalAction::Approval | ApprovalAction::AlwaysApproval => self.require(
                capability,
                PolicyReason::new(
                    "policy.git-requires-approval",
                    format!("git {name} requires approval"),
                ),
            ),
            ApprovalAction::Deny => self.deny(PolicyReason::new(
                "policy.git-denied",
                format!("git {name} is denied by policy"),
            )),
        }
    }

    fn allow(&self, capability: Capability, reason: PolicyReason) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![reason],
            capability_grant: Some(self.grant(capability)),
            policy_version: self.version.clone(),
        }
    }

    fn require(&self, capability: Capability, reason: PolicyReason) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::RequireApproval,
            reasons: vec![reason],
            capability_grant: Some(self.grant(capability)),
            policy_version: self.version.clone(),
        }
    }

    fn deny(&self, reason: PolicyReason) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Deny,
            reasons: vec![reason],
            capability_grant: None,
            policy_version: self.version.clone(),
        }
    }

    fn grant(&self, capability: Capability) -> CapabilityGrant {
        CapabilityGrant {
            capability,
            expires_at: Utc::now() + Duration::minutes(CAPABILITY_GRANT_TTL_MINUTES),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum GitOp {
    Commit,
    Push,
}

/// Build a canonical [`PathScope`] from unexpanded root/deny strings and a
/// context.
///
/// Failure directions differ by list. A root that cannot be expanded is
/// dropped — the scope only narrows, which is fail-closed. A DENY entry that
/// cannot be expanded must NOT be dropped: silently losing `$HOME/.ssh` in a
/// daemon started with a stripped environment would run with a *weaker* policy
/// than configured. Instead the whole scope is poisoned (no roots ⇒ every path
/// classifies `OutsideRoots` ⇒ reads/writes deny) until the environment can
/// honor the configured denials. Home is resolved via `$HOME` with an OS
/// fallback (`directories`), so the poison only triggers when the home
/// directory is genuinely unknowable.
fn build_path_scope(roots: &[String], deny: &[String], ctx: &EvalContext) -> PathScope {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()));
    build_path_scope_with_home(roots, deny, ctx, home.as_deref())
}

/// The pure core of [`build_path_scope`], with the home directory resolved by
/// the caller so the poisoning behaviour is testable without touching the
/// process environment. `home = None` models a daemon started with no
/// resolvable home.
fn build_path_scope_with_home(
    roots: &[String],
    deny: &[String],
    ctx: &EvalContext,
    home: Option<&Path>,
) -> PathScope {
    let canonical = |expanded: String| scope::canonicalize_lenient(Path::new(&expanded));

    let mut denies = Vec::with_capacity(deny.len());
    for entry in deny {
        match expand_vars(entry, ctx, home) {
            Some(expanded) => denies.push(canonical(expanded)),
            None => {
                tracing::error!(
                    entry,
                    "policy DENY entry is unresolvable ($HOME unknown); failing closed: \
                     all path access is refused until the daemon runs with a resolvable home"
                );
                return PathScope::new(Vec::new(), denies);
            }
        }
    }

    let expanded_roots = roots
        .iter()
        .filter_map(|entry| {
            let expanded = expand_vars(entry, ctx, home);
            if expanded.is_none() {
                // A dropped root only narrows the scope; still worth a loud note.
                tracing::warn!(entry, "policy root dropped: $HOME is unknown");
            }
            expanded
        })
        .map(canonical)
        .collect();

    PathScope::new(expanded_roots, denies)
}

/// Substitute `$REPOSITORY`, `$WORKTREE`, and `$HOME` in a raw path string.
/// Returns `None` when the string references `$HOME` but no home is available.
fn expand_vars(raw: &str, ctx: &EvalContext, home: Option<&Path>) -> Option<String> {
    let mut out = raw.to_string();
    if out.contains("$REPOSITORY") {
        out = out.replace("$REPOSITORY", &ctx.repository.to_string_lossy());
    }
    if out.contains("$WORKTREE") {
        out = out.replace("$WORKTREE", &ctx.worktree.to_string_lossy());
    }
    if out.contains("$HOME") {
        let path = home?;
        out = out.replace("$HOME", &path.to_string_lossy());
    }
    Some(out)
}

/// The hex SHA-256 of the merged policy's canonical JSON.
fn version_of(merged: &MergedPolicy) -> PolicyVersion {
    let canonical = serde_json::to_vec(merged).expect("merged policy serializes");
    let digest = Sha256::digest(&canonical);
    PolicyVersion(hex::encode(digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::ArtifactId;
    use tempfile::tempdir;

    fn ctx(repo: &Path, worktree: &Path) -> EvalContext {
        EvalContext::new(repo, worktree)
    }

    /// S4: a DENY entry that cannot be expanded (here `$HOME/.ssh` with no
    /// resolvable home) must POISON the whole scope — no roots, so every path
    /// classifies out of scope and both reads and writes are refused — rather
    /// than being silently dropped, which would run with a *weaker* policy than
    /// configured (fail-open on a deny is the exact hole this closes).
    #[test]
    fn unexpandable_deny_poisons_the_scope() {
        let dir = tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = ctx(&repo, &repo);

        // Roots that would normally allow the repository, plus a home-relative
        // deny the stripped environment cannot expand.
        let roots = vec!["$REPOSITORY".to_string()];
        let deny = vec!["$HOME/.ssh".to_string()];

        let scope = build_path_scope_with_home(&roots, &deny, &ctx, None);

        // Poisoned: no roots survived, so the deny could never be dropped.
        assert!(
            scope.roots.is_empty(),
            "an unresolvable deny must leave no allowed roots"
        );
        // A path that would be allowed under a healthy scope is now out of scope.
        assert_ne!(
            scope.classify(&repo.join("src.rs")),
            ScopeVerdict::Allowed,
            "every path must be refused while the deny is unresolvable"
        );
    }

    /// The contrast: with a resolvable home the same inputs build a *healthy*
    /// scope — the repository root is allowed and the `.ssh` deny is honoured —
    /// so the poisoning above is caused by the unresolvable deny, not the inputs.
    #[test]
    fn resolvable_home_builds_a_healthy_scope() {
        let dir = tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        let home = tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        let ctx = ctx(&repo, &repo);

        let roots = vec!["$REPOSITORY".to_string()];
        let deny = vec!["$HOME/.ssh".to_string()];

        let scope = build_path_scope_with_home(&roots, &deny, &ctx, Some(&home));

        assert!(!scope.roots.is_empty(), "the repository root must survive");
        assert_eq!(
            scope.classify(&repo.join("src.rs")),
            ScopeVerdict::Allowed,
            "an in-repository path is allowed under a healthy scope"
        );
        assert_eq!(
            scope.classify(&home.join(".ssh").join("id_rsa")),
            ScopeVerdict::Denied,
            "the resolved $HOME/.ssh deny is honoured"
        );
    }

    #[test]
    fn defaults_read_allows_in_repository() {
        let dir = tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(repo.join("src.rs"), b"code").unwrap();
        let engine = PolicyEngine::with_defaults();
        let decision = engine.evaluate(
            &ProposedAction::ReadFiles {
                paths: vec![repo.join("src.rs").to_string_lossy().into_owned()],
            },
            &ctx(&repo, &repo.join("wt")),
        );
        assert_eq!(decision.decision, Decision::Allow);
        assert!(decision.capability_grant.is_some());
    }

    #[test]
    fn command_requires_approval_and_rejects_unlisted() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let allowed = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(allowed.decision, Decision::RequireApproval);

        let denied = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "rm".to_string(),
                args: vec!["-rf".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(denied.decision, Decision::Deny);
    }

    #[test]
    fn network_denied_by_default_and_git_requires_approval() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let net = engine.evaluate(
            &ProposedAction::NetworkRequest {
                destination: "example.com:443".to_string(),
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(net.decision, Decision::Deny);

        let commit = engine.evaluate(
            &ProposedAction::GitCommit {
                repository: "repo".to_string(),
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(commit.decision, Decision::RequireApproval);
    }

    #[test]
    fn github_mutation_denied_without_network_grant() {
        // Built-in defaults have an empty network allow-list, so a GitHub write
        // is denied before it can even reach the approval gate.
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::GitHubMutation {
                repository: "octocat/hello-world".to_string(),
                summary: "create draft PR".to_string(),
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::Deny);
        assert_eq!(decision.reasons[0].code, "policy.github-network-denied");
    }

    #[test]
    fn github_mutation_requires_approval_when_endpoint_allowed() {
        // With the GitHub API endpoint on the network allow-list, a mutation is
        // permitted only through approval — every remote write is gated.
        let mut merged = MergedPolicy::builtin_defaults();
        merged.network_allow = vec![GITHUB_API_ENDPOINT.to_string()];
        let engine = PolicyEngine::from_merged(merged);
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::GitHubMutation {
                repository: "octocat/hello-world".to_string(),
                summary: "create draft PR".to_string(),
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::RequireApproval);
        assert_eq!(decision.reasons[0].code, "policy.github-requires-approval");
        assert!(matches!(
            decision.capability_grant.unwrap().capability,
            Capability::NetworkConnect(_)
        ));
    }

    #[test]
    fn github_mutation_denied_by_mode() {
        let mut merged = MergedPolicy::builtin_defaults();
        merged.network_allow = vec![GITHUB_API_ENDPOINT.to_string()];
        let engine = PolicyEngine::from_merged(merged);
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::GitHubMutation {
                repository: "octocat/hello-world".to_string(),
                summary: "create draft PR".to_string(),
            },
            &ctx(&repo, &repo).with_mode(ModeOverlay::read_only()),
        );
        assert_eq!(decision.decision, Decision::Deny);
        assert_eq!(decision.reasons[0].code, "policy.github-denied-by-mode");
    }

    #[test]
    fn explore_mode_cannot_write() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::WritePatch {
                patch: ArtifactId::new(),
            },
            &ctx(&repo, &repo).with_mode(ModeOverlay::read_only()),
        );
        assert_eq!(decision.decision, Decision::Deny);
        assert_eq!(decision.reasons[0].code, "policy.write-denied-by-mode");
    }

    #[test]
    fn build_mode_write_is_allowed_in_worktree() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let worktree = std::fs::canonicalize(dir.path()).unwrap();
        let decision = engine.evaluate(
            &ProposedAction::WritePatch {
                patch: ArtifactId::new(),
            },
            &ctx(&worktree.join("repo"), &worktree),
        );
        assert_eq!(decision.decision, Decision::Allow);
        assert!(matches!(
            decision.capability_grant.unwrap().capability,
            Capability::FileWrite(_)
        ));
    }

    #[test]
    fn policy_version_is_stable_and_sensitive() {
        let a = PolicyEngine::with_defaults();
        let b = PolicyEngine::with_defaults();
        assert_eq!(a.policy_version(), b.policy_version());

        let mut merged = MergedPolicy::builtin_defaults();
        merged.shell_maximum_seconds = 60;
        let c = PolicyEngine::from_merged(merged);
        assert_ne!(a.policy_version(), c.policy_version());
    }

    #[test]
    fn decision_round_trips_through_json() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "cargo".to_string(),
                args: Vec::new(),
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        let json = serde_json::to_string(&decision).unwrap();
        let parsed: PolicyDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decision, parsed);
    }
}
