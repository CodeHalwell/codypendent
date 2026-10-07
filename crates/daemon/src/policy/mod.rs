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

mod arity;
mod config;
mod scope;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use codypendent_protocol::ProposedAction;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use arity::{command_pattern, command_prefix, pattern_matches, UNLEARNABLE_PROGRAMS};
pub use config::{ApprovalAction, MergedPolicy, PolicyLoadError};
pub(crate) use scope::canonicalize_lenient;
pub use scope::{Capability, CommandScope, NetworkDefault, NetworkScope, PathScope, ScopeVerdict};

/// How long a minted capability grant remains valid. Capabilities are
/// invocation-scoped and time-limited (Chapter 11).
const CAPABILITY_GRANT_TTL_MINUTES: i64 = 15;

/// The `host:port` a GitHub mutation must be network-authorized against. GitHub
/// writes are network-scoped to exactly this endpoint (Phase 3 STEP 3.1).
pub const GITHUB_API_ENDPOINT: &str = "api.github.com:443";

/// The `host:port` a `web.search` call must be network-authorized against
/// (PR C1 — agent capabilities). Tavily searches are network-scoped to exactly
/// this endpoint; the executor admits it on the allow-list when a search client
/// is configured, exactly like [`GITHUB_API_ENDPOINT`].
pub const TAVILY_API_ENDPOINT: &str = "api.tavily.com:443";

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
    /// Whether a human `Run`-scoped approval may approve an identical later
    /// action. `AlwaysApproval` dispositions set this false.
    #[serde(default)]
    pub approval_reusable: bool,
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
    /// The one directory a `write_allowed == false` mode still permits writes
    /// under (adoption 19, Plan mode's `<worktree>/.codypendent/plans`).
    /// `None` for every mode without a plan-file exception. A write is allowed
    /// by this exception ONLY when [`write_target`](Self::write_target) resolves
    /// inside this root — deny-wins is untouched: this can never widen a deny
    /// that the file policy or another overlay imposes.
    pub plan_write_root: Option<PathBuf>,
    /// The resolved absolute path of the write under evaluation, when the
    /// caller knows it (the `workspace.write_file` / `workspace.edit_file`
    /// target). `None` for non-write actions and for multi-file patches, which
    /// therefore never qualify for the plan-file exception.
    pub write_target: Option<PathBuf>,
}

impl EvalContext {
    /// Context for a repository/worktree with no mode restriction.
    pub fn new(repository: impl Into<PathBuf>, worktree: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
            worktree: worktree.into(),
            mode: ModeOverlay::permissive(),
            plan_write_root: None,
            write_target: None,
        }
    }

    /// Set the mode overlay.
    pub fn with_mode(mut self, mode: ModeOverlay) -> Self {
        self.mode = mode;
        self
    }

    /// Set the plan-file write root (adoption 19). See [`plan_write_root`].
    #[must_use]
    pub fn with_plan_write_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.plan_write_root = Some(root.into());
        self
    }

    /// Set the resolved write target for this evaluation (adoption 19).
    #[must_use]
    pub fn with_write_target(mut self, target: impl Into<PathBuf>) -> Self {
        self.write_target = Some(target.into());
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
    /// defaults, applying each layer through the merge path its trust level
    /// demands (PF4 — see the policy-files design spec, Decision 1):
    ///
    /// - `global_policy` (the User layer, the operator's own machine) is
    ///   **trusted** and goes through [`MergedPolicy::apply_trusted_overlay`] —
    ///   it may widen the shell/network allow-lists and `fs_read`, and relax
    ///   git/network approval dispositions (never `fs_write`; see Decision 2a).
    /// - `repo_policy` (the Repository layer, `<repo>/.codypendent/policy.toml`,
    ///   possibly attacker-controlled) is **untrusted** and goes through
    ///   [`MergedPolicy::apply_untrusted_overlay`] — narrow-only, applied LAST
    ///   so it can always claw back what the global layer widened but can
    ///   never exceed it.
    ///
    /// A `None` path — or a path that does not exist — is skipped (the layer
    /// contributes nothing, exactly as if the file were absent). A malformed
    /// file (including an unknown key) is an `Err`: never silently ignored,
    /// and never silently defaulted — a caller must fail the run rather than
    /// fall back to `with_defaults`, since that would be a silent widen back
    /// to (weaker) built-ins for a layer that was meant to narrow or that the
    /// user meant to widen deliberately.
    ///
    /// Passing explicit paths keeps the engine testable without reading real
    /// user directories.
    pub fn load(
        repo_policy: Option<&Path>,
        global_policy: Option<&Path>,
    ) -> Result<Self, PolicyLoadError> {
        let mut merged = MergedPolicy::builtin_defaults();
        if let Some(path) = global_policy {
            if let Some(raw) = config::load_layer(path)? {
                merged.apply_trusted_overlay(&raw);
            }
        }
        if let Some(path) = repo_policy {
            if let Some(raw) = config::load_layer(path)? {
                merged.apply_untrusted_overlay(&raw);
            }
        }
        Ok(Self::from_merged(merged))
    }

    /// Widen an ALREADY-MERGED engine's network allow-list, admitting
    /// `endpoints` on top of whatever [`load`](Self::load) (or
    /// [`with_defaults`](Self::with_defaults)) produced. Used to re-admit
    /// [`GITHUB_API_ENDPOINT`] after loading policy files, so a configured
    /// GitHub client's endpoint composes with a loaded policy exactly as
    /// [`with_defaults_allowing_network`](Self::with_defaults_allowing_network)
    /// composes it over the built-in defaults. Admitting an endpoint grants
    /// nothing on its own — every GitHub write still returns
    /// `RequireApproval` ([`eval_github_mutation`](Self::eval_github_mutation)).
    pub fn admitting_network(mut self, endpoints: impl IntoIterator<Item = String>) -> Self {
        self.merged.network_allow.extend(endpoints);
        Self::from_merged(self.merged)
    }

    fn from_merged(merged: MergedPolicy) -> Self {
        let version = version_of(&merged);
        Self { merged, version }
    }

    /// The version identifying this engine's merged policy.
    pub fn policy_version(&self) -> &PolicyVersion {
        &self.version
    }

    /// The merged, fully-resolved policy, read-only. Exposed for operator-facing
    /// surfaces — `codypendent mcp list` reports the effective `[mcp]`
    /// dispositions from it — never for evaluation (that goes through the
    /// `eval_*` methods, which bind dispositions to capabilities + reasons).
    pub fn merged(&self) -> &MergedPolicy {
        &self.merged
    }

    /// The read scope for `ctx`, with `$REPOSITORY`/`$WORKTREE`/`$HOME`
    /// expanded and every root canonicalized. Exposed so the tool layer can
    /// check specific paths under a granted capability.
    pub fn file_read_scope(&self, ctx: &EvalContext) -> PathScope {
        build_path_scope(
            &self.merged.fs_read,
            &self.merged.fs_deny,
            &self.merged.untrusted_fs_roots,
            ctx,
        )
    }

    /// The write scope for `ctx` (see [`file_read_scope`]).
    ///
    /// [`file_read_scope`]: PolicyEngine::file_read_scope
    pub fn file_write_scope(&self, ctx: &EvalContext) -> PathScope {
        build_path_scope(
            &self.merged.fs_write,
            &self.merged.fs_deny,
            &self.merged.untrusted_fs_roots,
            ctx,
        )
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
            ProposedAction::ExecuteCommand { program, args, .. } => {
                self.eval_command(program, args, ctx)
            }
            ProposedAction::NetworkRequest { destination } => self.eval_network(destination, ctx),
            ProposedAction::GitCommit { .. } => self.eval_git(GitOp::Commit, ctx),
            ProposedAction::GitPush { .. } => self.eval_git(GitOp::Push, ctx),
            ProposedAction::GitHubMutation { .. } => self.eval_github_mutation(ctx),
            ProposedAction::McpToolCall { server, .. } => self.eval_mcp_tool_call(server, ctx),
            // The same reasoning as a blackboard access, for the same reason: a
            // workflow-graph read and a task-board write touch only Codypendent's
            // own durable coordination state, never the filesystem, a command, or
            // a remote (rubrics 5 and 10).
            ProposedAction::BlackboardPost { .. }
            | ProposedAction::BlackboardQuery { .. }
            | ProposedAction::WorkflowQuery { .. }
            | ProposedAction::TaskWrite { .. }
            | ProposedAction::TaskRead { .. }
            | ProposedAction::CouncilResultRead { .. }
            // Outcome 5: a code-graph query reads only the derived projection of
            // this repository that the daemon itself parsed — the same "internal
            // state, no external effect" reasoning as the reads above.
            | ProposedAction::CodeGraphQuery { .. }
            // An agent edge assertion WRITES that same derived projection, and is
            // allowed on the same reasoning as a task-board write: it touches no
            // file, command, network or remote, it cannot create a node (both
            // endpoints must already exist), and it cannot overwrite a
            // higher-confidence fact — the store enforces that, not this gate.
            | ProposedAction::CodeGraphAssert { .. } => self.eval_blackboard(),
            // Council creation changes durable configuration, and running one
            // fans out model requests with potentially material cost. Both are
            // deliberately fresh approvals; the ProposedAction is the preview.
            ProposedAction::CouncilCreate { .. } | ProposedAction::CouncilRun { .. } => self
                .require_once(
                    Capability::CouncilManage,
                    PolicyReason::new(
                        "policy.council-requires-approval",
                        "creating or running an agent council requires explicit approval",
                    ),
                ),
            // Creating a workflow writes durable user configuration; running one
            // can fan out model/tool work and incur cost. Both require a fresh,
            // non-reusable approval with the typed manifest preview.
            ProposedAction::WorkflowCreate { .. } | ProposedAction::WorkflowRun { .. } => self
                .require_once(
                    Capability::WorkflowManage,
                    PolicyReason::new(
                        "policy.workflow-requires-approval",
                        "creating or running a workflow requires explicit approval",
                    ),
                ),
            // Writing stdin to a process the run already spawned carries the same
            // authority as spawning a command — it can drive arbitrary execution
            // inside an approved interactive child — so it is honestly evaluated as
            // a command execution and ALWAYS approval-gated (never silently
            // allowed). It must never fall through to `eval_read` on an empty path
            // set, which would auto-`Allow` it as a no-op file read.
            ProposedAction::WriteProcessStdin { .. } => self.eval_write_process_stdin(ctx),
            ProposedAction::RecordMemory => self.eval_record_memory(),
            ProposedAction::SearchRegistry => self.eval_search_registry(),
            ProposedAction::DocumentEdit { .. } => self.eval_document_edit(),
            ProposedAction::AskUser { .. } => self.eval_ask_user(),
            ProposedAction::PlanTransition { .. } => self.eval_plan_transition(),
            ProposedAction::RestoreCheckpoint { .. } => self.eval_restore_checkpoint(),
            ProposedAction::ReadSecret { name } => self.eval_read_secret(name),
            _ => self.deny(PolicyReason::new(
                "policy.unsupported-action",
                "the proposed action is not recognized by this policy engine",
            )),
        }
    }

    /// A blackboard post/query (Phase 5 STEP 5.3) — and, on the same reasoning, a
    /// `workflow.query` read or a `task.*` board write (rubrics 5/10) — is always
    /// permitted: each targets only Codypendent's OWN durable coordination state,
    /// not the filesystem, the repository, or any remote. It grants no capability
    /// (none of these tools needs a path/command/network scope) and is recorded
    /// purely so the access is traced like any other tool call. Writes that DO
    /// escape the run (files, git, GitHub) keep their existing approval gates; this
    /// does not widen them.
    fn eval_blackboard(&self) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![PolicyReason::new(
                "policy.blackboard-allowed",
                "a workflow blackboard access targets only the run's own artifact channel",
            )],
            capability_grant: None,
            policy_version: self.version.clone(),
            approval_reusable: false,
        }
    }

    /// A `memory.remember` call (smarter-memory M2) is always permitted: its
    /// only effect is a `NoteAppended` on the run's own ledger — not the
    /// filesystem, a command, the network, or any remote — so it grants no
    /// capability and never reaches the approval gate, exactly like a
    /// blackboard access.
    fn eval_record_memory(&self) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![PolicyReason::new(
                "policy.record-memory-allowed",
                "a memory proposal note targets only the run's own ledger",
            )],
            capability_grant: None,
            policy_version: self.version.clone(),
            approval_reusable: false,
        }
    }

    /// A `skills.search` call (rubric 9) is always permitted, for the same reason
    /// as [`Self::eval_record_memory`]: it READS the daemon's own registry —
    /// never the filesystem, a command, the network, or a remote — so it grants
    /// no capability and never reaches the approval gate. A skill's package
    /// directory comes from its registry row, so no model-supplied path is ever
    /// opened and the file-read scope has nothing to enforce here.
    fn eval_search_registry(&self) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![PolicyReason::new(
                "policy.search-registry-allowed",
                "a registry search reads only the daemon's own tool/skill catalog",
            )],
            capability_grant: None,
            policy_version: self.version.clone(),
            approval_reusable: false,
        }
    }

    /// An agent `docs.*` access (rubric #4 doc writer) is always permitted: it
    /// targets only the knowledge fabric's document store — not the filesystem,
    /// a command, the network, or any remote. It grants no capability (the tools
    /// need no path/command/network scope) and is recorded purely so the access
    /// is traced, exactly like [`eval_record_memory`](Self::eval_record_memory).
    ///
    /// What an agent may actually DO to a document is not this engine's gate: it
    /// is the document's own collaboration mode (organization scope defaults to
    /// *Suggest*, so an agent edit lands as a reviewable suggestion), enforced in
    /// the knowledge crate's `apply_mutation`. Publishing a document to Git keeps
    /// its separate approval gate (`ProposedAction::PublishDocument`); this does
    /// not widen it.
    fn eval_document_edit(&self) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![PolicyReason::new(
                "policy.document-access-allowed",
                "a collaborative document edit targets only the store and is still gated by \
                 the document's collaboration mode",
            )],
            capability_grant: None,
            policy_version: self.version.clone(),
            approval_reusable: false,
        }
    }

    /// A `user.ask` call (adoption 03) is always permitted: it asks the
    /// session's own human a question — no filesystem, command, network, or
    /// remote effect — so it grants no capability and never reaches the
    /// approval gate, exactly like a memory proposal note.
    fn eval_ask_user(&self) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![PolicyReason::new(
                "policy.ask-user-allowed",
                "a question to the operator targets only the session's own human",
            )],
            capability_grant: None,
            policy_version: self.version.clone(),
            approval_reusable: false,
        }
    }

    /// A `plan_enter`/`plan_exit` call (adoption 19) is always permitted: like
    /// `user.ask` it targets only the session's own human (a yes/no question)
    /// and the session's own durable prompt queue — no filesystem, command,
    /// network, or remote effect — so it grants no capability and never reaches
    /// the approval gate.
    fn eval_plan_transition(&self) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![PolicyReason::new(
                "policy.plan-transition-allowed",
                "a plan-mode transition targets only the session's own human and queue",
            )],
            capability_grant: None,
            policy_version: self.version.clone(),
            approval_reusable: false,
        }
    }

    /// A brokered secret read (Milestone 5) is policy-allowed within the
    /// manifest declaration ceiling; secret material is resolved per call at
    /// the final transport boundary without exposing material to guest code.
    fn eval_read_secret(&self, name: &str) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Allow,
            reasons: vec![PolicyReason::new(
                "policy.secret-brokered",
                format!("secret `{name}` is brokered per call"),
            )],
            capability_grant: None,
            policy_version: self.version.clone(),
            approval_reusable: false,
        }
    }

    /// A checkpoint restore (Adoption 04) is destructive for uncheckpointed
    /// work done after the snapshot, so it ALWAYS requires explicit single-use
    /// human approval (never auto-approvable across runs).
    fn eval_restore_checkpoint(&self) -> PolicyDecision {
        self.require_once(
            Capability::RestoreCheckpoint,
            PolicyReason::new(
                "policy.checkpoint-restore-requires-approval",
                "restoring a worktree checkpoint requires explicit human approval",
            ),
        )
    }

    /// Writing stdin to an already-running process (the `shell.write_stdin`
    /// unified-exec tool). Bytes sent to a live child's stdin can drive arbitrary
    /// execution inside it, so the write carries the same authority as spawning a
    /// command: it is denied outright in modes that forbid command execution, and
    /// otherwise ALWAYS requires explicit approval (single-use — the payload is not
    /// carried on the action, so no reusable digest can safely stand in for
    /// distinct writes). It is never silently allowed.
    fn eval_write_process_stdin(&self, ctx: &EvalContext) -> PolicyDecision {
        if !ctx.mode.command_allowed {
            return self.deny(PolicyReason::new(
                "policy.command-denied-by-mode",
                "the active mode forbids command execution, including process stdin",
            ));
        }
        self.require_once(
            Capability::CommandExecute(self.command_scope()),
            PolicyReason::new(
                "policy.process-stdin-requires-approval",
                "writing to a running process's stdin requires explicit approval, like \
                 executing a command",
            ),
        )
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
            // Adoption 19 — Plan mode's single write exception: a write whose
            // resolved target is inside the run's plan directory is permitted
            // even though the mode otherwise forbids all writes. Everything
            // else stays denied. Deny-wins is preserved: this only ADDS an
            // allow inside a branch that would otherwise deny, and the grant is
            // scoped to exactly the plan directory.
            if let (Some(plan_root), Some(target)) =
                (ctx.plan_write_root.as_deref(), ctx.write_target.as_deref())
            {
                let plan_scope = PathScope::new(vec![plan_root.to_path_buf()], Vec::new());
                if plan_scope.allows(target) {
                    return self.allow(
                        Capability::FileWrite(plan_scope),
                        PolicyReason::new(
                            "policy.plan-file-write-allowed",
                            "Plan mode permits writing the plan file under \
                             .codypendent/plans/",
                        ),
                    );
                }
            }
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

    fn eval_command(&self, program: &str, args: &[String], ctx: &EvalContext) -> PolicyDecision {
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
        if program == "git" {
            let subcommand = match git_subcommand(args) {
                Ok(subcommand) => subcommand,
                Err(message) => {
                    return self.deny(PolicyReason::new(
                        "policy.git-global-option-unsupported",
                        message,
                    ));
                }
            };
            let mutating = matches!(
                subcommand,
                Some(
                    "add"
                        | "am"
                        | "apply"
                        | "branch"
                        | "checkout"
                        | "cherry-pick"
                        | "clean"
                        | "commit"
                        | "merge"
                        | "mv"
                        | "rebase"
                        | "reset"
                        | "restore"
                        | "revert"
                        | "rm"
                        | "stash"
                        | "switch"
                        | "tag"
                        | "worktree"
                )
            );
            if mutating && !ctx.mode.write_allowed {
                return self.deny(PolicyReason::new(
                    "policy.git-shell-write-denied-by-mode",
                    "the active mode forbids repository mutations through shell git",
                ));
            }
            let networked = matches!(
                subcommand,
                Some("clone" | "fetch" | "pull" | "push" | "ls-remote" | "submodule")
            );
            if networked && !ctx.mode.network_allowed {
                return self.deny(PolicyReason::new(
                    "policy.git-shell-network-denied-by-mode",
                    "the active mode forbids networked git commands",
                ));
            }

            // A leading `+` on a refspec forces the push exactly as `--force`
            // does — `git push origin +main` is a force push — and options never
            // begin with `+`, so any such argument is a forcing refspec.
            // Without it, `git_force_push` (default Deny) was bypassed entirely
            // and the command fell through to the generic reusable-approval
            // path, where one approved `git push *` pattern then auto-approved
            // force pushes for good.
            let force_push = subcommand == Some("push")
                && args.iter().any(|arg| {
                    matches!(
                        arg.as_str(),
                        "-f" | "--force" | "--force-with-lease" | "--force-if-includes"
                    ) || arg.starts_with("--force-with-lease=")
                        || arg.starts_with('+')
                });
            if force_push {
                return self.command_disposition(self.merged.git_force_push, scope, "force-push");
            }
            let delete_branch = (subcommand == Some("branch")
                && args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "-d" | "-D" | "--delete")))
                // `-d` is git's shorthand for `--delete` on push just as it is
                // on branch, and the `branch` arm above already matched it —
                // the two halves of one check disagreed about the same flag.
                || (subcommand == Some("push")
                    && args
                        .iter()
                        .any(|arg| arg == "--delete" || arg == "-d" || arg.starts_with(':')));
            if delete_branch {
                return self.command_disposition(
                    self.merged.git_delete_branch,
                    scope,
                    "branch deletion",
                );
            }
        }

        // Adoption 07: scan path-like arguments of file-manipulating commands
        // against the run's path scopes. Strictly narrowing: an in-scope path
        // changes nothing; a denied path denies; an out-of-scope path forces a
        // fresh, non-reusable approval that NAMES the external directories.
        if let Some(hit) = scan_command_paths(program, args, self, ctx) {
            match hit {
                PathScanHit::Denied(path) => {
                    return self.deny(PolicyReason::new(
                        "policy.command-path-denied",
                        format!(
                            "`{program}` targets `{path}`, which is on the filesystem deny list",
                        ),
                    ));
                }
                PathScanHit::External(dirs) => {
                    return self.require_once(
                        Capability::CommandExecute(scope),
                        PolicyReason::new(
                            "policy.command-touches-external-paths",
                            format!(
                                "`{program}` touches paths outside the granted roots: {}",
                                dirs.join(", ")
                            ),
                        ),
                    );
                }
            }
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

    fn command_disposition(
        &self,
        disposition: ApprovalAction,
        scope: CommandScope,
        operation: &str,
    ) -> PolicyDecision {
        let capability = Capability::CommandExecute(scope);
        match disposition {
            ApprovalAction::Allow | ApprovalAction::Approval => self.require(
                capability,
                PolicyReason::new(
                    "policy.git-shell-requires-approval",
                    format!("git {operation} requires approval"),
                ),
            ),
            ApprovalAction::AlwaysApproval => self.require_once(
                capability,
                PolicyReason::new(
                    "policy.git-shell-always-requires-approval",
                    format!("git {operation} requires a fresh approval every time"),
                ),
            ),
            ApprovalAction::Deny => self.deny(PolicyReason::new(
                "policy.git-shell-denied",
                format!("git {operation} is denied by policy"),
            )),
        }
    }
}

/// File-manipulating commands whose arguments are worth resolving as paths —
/// the opencode FILES set minus shells/PowerShell (no such invocations here).
const PATH_SCANNED_PROGRAMS: &[&str] = &[
    "cd", "rm", "cp", "mv", "mkdir", "rmdir", "touch", "chmod", "chown", "cat", "head", "tail",
    "ln",
];

enum PathScanHit {
    Denied(String),
    External(Vec<String>),
}

/// Port of shell.ts `prefix()`: first of `?*[` — at index 0 ⇒ `None`, else the slice before it; no match ⇒ the whole arg.
fn glob_literal_prefix(arg: &str) -> Option<&str> {
    if let Some(idx) = arg.find(['?', '*', '[']) {
        if idx == 0 {
            None
        } else {
            Some(&arg[..idx])
        }
    } else {
        Some(arg)
    }
}

/// Expand leading `~` or `$HOME` (the same names expand_roots already honors).
fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()));
    if path == "~" {
        home.unwrap_or_else(|| PathBuf::from("~"))
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else if path == "$HOME" {
        home.unwrap_or_else(|| PathBuf::from(path))
    } else if let Some(rest) = path.strip_prefix("$HOME/") {
        home.map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    }
}

/// Port of shell.ts `argPath`/`pathArgs`/`prefix`, simplified for structured
/// args: skip `-`-flags (and chmod's `+x` modes); expand a leading `~`; trim a
/// glob tail to its literal prefix (a leading glob char ⇒ skip the arg);
/// resolve against `ctx.worktree`; classify with the union of the read and
/// write scopes (deny list included via the scopes themselves).
fn scan_command_paths(
    program: &str,
    args: &[String],
    engine: &PolicyEngine,
    ctx: &EvalContext,
) -> Option<PathScanHit> {
    if !PATH_SCANNED_PROGRAMS.contains(&program) {
        return None;
    }
    let read = engine.file_read_scope(ctx);
    let write = engine.file_write_scope(ctx);
    let mut external: std::collections::BTreeSet<String> = Default::default();
    for arg in args {
        if arg.starts_with('-') || (program == "chmod" && arg.starts_with('+')) {
            continue;
        }
        let Some(text) = glob_literal_prefix(arg) else {
            continue;
        };
        let expanded = expand_home(text);
        let resolved = if expanded.is_absolute() {
            expanded
        } else {
            ctx.worktree.join(expanded)
        };
        match (read.classify(&resolved), write.classify(&resolved)) {
            (ScopeVerdict::Denied, _) | (_, ScopeVerdict::Denied) => {
                return Some(PathScanHit::Denied(resolved.display().to_string()));
            }
            (ScopeVerdict::OutsideRoots, ScopeVerdict::OutsideRoots) => {
                let dir = if resolved.is_dir() {
                    resolved.clone()
                } else {
                    resolved
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or(resolved.clone())
                };
                external.insert(dir.display().to_string());
            }
            _ => {}
        }
    }
    (!external.is_empty()).then(|| PathScanHit::External(external.into_iter().collect()))
}

impl PolicyEngine {
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

    /// Evaluate a tool call to an external MCP server (PR B — MCP client). The
    /// server is operator-declared in the trusted `mcp.toml` — a name the model
    /// invents fails earlier, in `prepare` — and the `[mcp]` policy section
    /// dispositions each call: allow-listed servers run ungated, everything else
    /// is approval-gated or denied. A call's effect is arbitrary (the server is
    /// an external process), so it is mode-gated like command execution: a
    /// read-only mode forbids it. The minted grant is a marker `McpToolCall`
    /// capability — the MCP bridge executes the call itself and needs no
    /// path/command/network scope.
    fn eval_mcp_tool_call(&self, server: &str, ctx: &EvalContext) -> PolicyDecision {
        if !ctx.mode.command_allowed {
            return self.deny(PolicyReason::new(
                "policy.mcp-denied-by-mode",
                "the active mode forbids command execution, and an MCP tool call is an \
                 external effect",
            ));
        }
        let disposition = self
            .merged
            .mcp_servers
            .get(server)
            .copied()
            .unwrap_or(self.merged.mcp_default);
        let capability = Capability::McpToolCall {
            server: server.to_string(),
        };
        match disposition {
            ApprovalAction::Allow => self.allow(
                capability,
                PolicyReason::new(
                    "policy.mcp-allowed",
                    format!("MCP server `{server}` is allow-listed by policy"),
                ),
            ),
            ApprovalAction::Approval => self.require(
                capability,
                PolicyReason::new(
                    "policy.mcp-requires-approval",
                    format!("MCP tool calls to `{server}` require approval"),
                ),
            ),
            ApprovalAction::AlwaysApproval => self.require_once(
                capability,
                PolicyReason::new(
                    "policy.mcp-always-requires-approval",
                    format!("MCP tool calls to `{server}` require a fresh approval every time"),
                ),
            ),
            ApprovalAction::Deny => self.deny(PolicyReason::new(
                "policy.mcp-denied",
                format!("MCP server `{server}` is denied by policy"),
            )),
        }
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
            ApprovalAction::Approval => self.require(
                capability,
                PolicyReason::new(
                    "policy.git-requires-approval",
                    format!("git {name} requires approval"),
                ),
            ),
            ApprovalAction::AlwaysApproval => self.require_once(
                capability,
                PolicyReason::new(
                    "policy.git-always-requires-approval",
                    format!("git {name} requires a fresh approval every time"),
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
            approval_reusable: false,
        }
    }

    fn require(&self, capability: Capability, reason: PolicyReason) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::RequireApproval,
            reasons: vec![reason],
            capability_grant: Some(self.grant(capability)),
            policy_version: self.version.clone(),
            approval_reusable: true,
        }
    }

    fn require_once(&self, capability: Capability, reason: PolicyReason) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::RequireApproval,
            reasons: vec![reason],
            capability_grant: Some(self.grant(capability)),
            policy_version: self.version.clone(),
            approval_reusable: false,
        }
    }

    fn deny(&self, reason: PolicyReason) -> PolicyDecision {
        PolicyDecision {
            decision: Decision::Deny,
            reasons: vec![reason],
            capability_grant: None,
            policy_version: self.version.clone(),
            approval_reusable: false,
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

/// Locate the actual git subcommand after parsing the documented global-option
/// prefix. Options such as `-C <path>` and `-c <name=value>` consume a following
/// non-option argument, so looking for the first argument that does not start
/// with `-` mistakes the option's value for the subcommand and bypasses the
/// mutation/network/force-push policy. Unknown global options fail closed: a
/// future option may also consume an operand, and guessing would recreate the
/// same policy hole.
fn git_subcommand(args: &[String]) -> Result<Option<&str>, String> {
    let mut index = 0;
    while let Some(arg) = args.get(index).map(String::as_str) {
        if arg == "--" {
            return Ok(args.get(index + 1).map(String::as_str));
        }
        if !arg.starts_with('-') || arg == "-" {
            return Ok(Some(arg));
        }

        let consumes_next = matches!(
            arg,
            "-C" | "-c"
                | "--config-env"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--attr-source"
        );
        if consumes_next {
            if args.get(index + 1).is_none() {
                return Err(format!("git global option `{arg}` is missing its value"));
            }
            index += 2;
            continue;
        }

        let attached_value = (arg.starts_with("-C") || arg.starts_with("-c")) && arg.len() > 2;
        let long_value = [
            "--config-env=",
            "--exec-path=",
            "--git-dir=",
            "--work-tree=",
            "--namespace=",
            "--super-prefix=",
            "--attr-source=",
            "--list-cmds=",
        ]
        .iter()
        .any(|prefix| arg.starts_with(prefix));
        if attached_value || long_value {
            index += 1;
            continue;
        }

        let flag = matches!(
            arg,
            "-p" | "-P"
                | "-h"
                | "--paginate"
                | "--no-pager"
                | "--no-replace-objects"
                | "--bare"
                | "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
                | "--no-optional-locks"
                | "--no-lazy-fetch"
                | "--no-advice"
                | "--version"
                | "--help"
                | "--exec-path"
                | "--html-path"
                | "--man-path"
                | "--info-path"
        );
        if flag {
            index += 1;
            continue;
        }

        return Err(format!(
            "unsupported git global option `{arg}`; refusing to guess the subcommand"
        ));
    }
    Ok(None)
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
fn build_path_scope(
    roots: &[String],
    deny: &[String],
    untrusted_roots: &[String],
    ctx: &EvalContext,
) -> PathScope {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()));
    build_path_scope_with_home(roots, deny, untrusted_roots, ctx, home.as_deref())
}

/// The pure core of [`build_path_scope`], with the home directory resolved by
/// the caller so the poisoning behaviour is testable without touching the
/// process environment. `home = None` models a daemon started with no
/// resolvable home.
fn build_path_scope_with_home(
    roots: &[String],
    deny: &[String],
    untrusted_roots: &[String],
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
            expanded.map(|expanded| (entry, canonical(expanded)))
        })
        .filter(|(entry, resolved)| {
            // A root an untrusted layer introduced is a path the REPOSITORY
            // chose, and the repository can plant a symlink there. Resolving it
            // follows the link, so `$WORKTREE/link -> /` would otherwise turn
            // into a root of `/`: silent read/write access to the whole machine
            // minus the deny list, with no approval card. Hold it to its own
            // anchor — after symlink resolution it must still be inside the
            // worktree / repository / home directory it was written relative to.
            // Dropping a root only narrows the scope, so this fails closed.
            if !untrusted_roots.iter().any(|untrusted| untrusted == *entry) {
                return true;
            }
            let anchor = if entry.starts_with("$WORKTREE") {
                Some(ctx.worktree.as_path())
            } else if entry.starts_with("$REPOSITORY") {
                Some(ctx.repository.as_path())
            } else if entry.starts_with("$HOME") {
                home
            } else {
                None
            };
            let contained = anchor
                .map(|anchor| scope::is_within(resolved, &scope::canonicalize_lenient(anchor)))
                .unwrap_or(false);
            if !contained {
                tracing::error!(
                    root = entry.as_str(),
                    resolves_to = %resolved.display(),
                    "dropping a repository-supplied policy root that resolves outside its own \
                     anchor (a symlink leading out of the checkout); the scope narrows"
                );
            }
            contained
        })
        .map(|(_, resolved)| resolved)
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

    /// A repository controls two things an attacker needs: the files in its
    /// checkout (so it can commit a symlink) and its own `.codypendent/policy.toml`
    /// (so it can name that symlink as a policy root). Narrowing `$WORKTREE` to
    /// `$WORKTREE/link` passes the lexical containment check at merge time; the
    /// evaluator then canonicalised the root THROUGH the link, so a root of `/`
    /// (or `~/.aws`) was granted with no approval card — read access to every
    /// credential file not on the deny list, write access to `~/.bashrc`.
    #[cfg(unix)]
    #[test]
    fn a_repository_policy_root_that_is_a_symlink_out_of_the_checkout_is_dropped() {
        use crate::policy::config::RawPolicy;

        let outside = tempdir().unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        std::fs::write(outside.join("credentials"), "aws_secret_access_key=...").unwrap();
        let checkout = tempdir().unwrap();
        let worktree = std::fs::canonicalize(checkout.path()).unwrap();
        std::os::unix::fs::symlink(&outside, worktree.join("link")).unwrap();
        std::fs::create_dir(worktree.join("src")).unwrap();
        std::fs::write(worktree.join("src/lib.rs"), "fn main() {}").unwrap();

        let mut merged = MergedPolicy::builtin_defaults();
        merged.apply_untrusted_overlay(
            &RawPolicy::parse(
                "[filesystem]\nread = [\"$REPOSITORY/link\"]\nwrite = [\"$WORKTREE/link\"]",
            )
            .unwrap(),
        );
        let engine = PolicyEngine::from_merged(merged);
        let ctx = ctx(&worktree, &worktree);

        let secret = outside.join("credentials");
        assert_ne!(
            engine.file_write_scope(&ctx).classify(&secret),
            ScopeVerdict::Allowed,
            "a symlinked write root must not grant the directory it points at"
        );
        assert_ne!(
            engine.file_read_scope(&ctx).classify(&secret),
            ScopeVerdict::Allowed,
            "a symlinked read root must not grant the directory it points at"
        );
    }

    /// The other half: a root the repository narrowed to that genuinely lies
    /// inside the checkout still works, symlink or not.
    #[cfg(unix)]
    #[test]
    fn a_repository_policy_root_that_stays_inside_the_checkout_is_kept() {
        use crate::policy::config::RawPolicy;

        let checkout = tempdir().unwrap();
        let worktree = std::fs::canonicalize(checkout.path()).unwrap();
        std::fs::create_dir_all(worktree.join("real/inner")).unwrap();
        std::fs::write(worktree.join("real/inner/file.rs"), "").unwrap();
        // An alias that stays inside the checkout.
        std::os::unix::fs::symlink(worktree.join("real"), worktree.join("alias")).unwrap();

        let mut merged = MergedPolicy::builtin_defaults();
        merged.apply_untrusted_overlay(
            &RawPolicy::parse("[filesystem]\nwrite = [\"$WORKTREE/real\", \"$WORKTREE/alias\"]")
                .unwrap(),
        );
        let engine = PolicyEngine::from_merged(merged);
        let ctx = ctx(&worktree, &worktree);
        let scope = engine.file_write_scope(&ctx);

        assert_eq!(
            scope.classify(&worktree.join("real/inner/file.rs")),
            ScopeVerdict::Allowed
        );
        // Outside the narrowed roots is still outside.
        assert_ne!(
            scope.classify(&worktree.join("elsewhere.txt")),
            ScopeVerdict::Allowed
        );
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

        let scope = build_path_scope_with_home(&roots, &deny, &[], &ctx, None);

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

        let scope = build_path_scope_with_home(&roots, &deny, &[], &ctx, Some(&home));

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

    /// FIX 1 (policy bypass): `shell.write_stdin` must NOT lower to an empty
    /// `ReadFiles`, which `eval_read` auto-`Allow`s by skipping its loop. A stdin
    /// write to a live process carries command authority, so the dedicated
    /// `WriteProcessStdin` action is approval-gated and grants a CommandExecute
    /// capability — the decision reflects a process write, not a no-op file read.
    /// An actually-empty `ReadFiles` must remain legitimately allowed (the fix
    /// lives at the lowering site, not in `eval_read`).
    #[test]
    fn write_process_stdin_requires_approval_not_a_silent_read() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();

        let decision = engine.evaluate(
            &ProposedAction::WriteProcessStdin {
                process_id: 4242,
                byte_len: 16,
            },
            &ctx(&repo, &repo),
        );

        assert_eq!(
            decision.decision,
            Decision::RequireApproval,
            "writing to a process stdin must be approval-gated, never auto-allowed"
        );
        assert!(
            decision
                .reasons
                .iter()
                .any(|r| r.code == "policy.process-stdin-requires-approval"),
            "the decision must reflect a process-write reason, not a file read"
        );
        assert!(
            matches!(
                decision.capability_grant.as_ref().map(|g| &g.capability),
                Some(Capability::CommandExecute(_))
            ),
            "the write must be gated by command-execute authority, not FileRead"
        );

        // An actually-empty ReadFiles is still legitimately allowed.
        let empty_read = engine.evaluate(
            &ProposedAction::ReadFiles { paths: Vec::new() },
            &ctx(&repo, &repo),
        );
        assert_eq!(empty_read.decision, Decision::Allow);
    }

    /// FIX 2 (agent & tool fixes spec, §2a + Reconciliation R2): widening the
    /// default allow-list moves a curated command from Deny to
    /// **RequireApproval**, never to auto-run — `eval_command` requires approval
    /// for every allow-listed program. `find` (excluded — `-delete`/`-exec`) and
    /// an interpreter (`python`, never added) must still deny outright.
    #[test]
    fn newly_curated_command_requires_approval_while_find_and_interpreters_stay_denied() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();

        let ls = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "ls".to_string(),
                args: Vec::new(),
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(
            ls.decision,
            Decision::RequireApproval,
            "a newly curated command still requires approval, never auto-runs"
        );

        for program in ["find", "python"] {
            let denied = engine.evaluate(
                &ProposedAction::ExecuteCommand {
                    program: program.to_string(),
                    args: Vec::new(),
                    environment: Vec::new(),
                    cwd: None,
                },
                &ctx(&repo, &repo),
            );
            assert_eq!(
                denied.decision,
                Decision::Deny,
                "`{program}` must stay denied"
            );
            assert_eq!(denied.reasons[0].code, "policy.program-not-allowlisted");
        }
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
    fn git_global_options_cannot_hide_a_mutating_subcommand() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "git".to_string(),
                args: vec![
                    "-C".to_string(),
                    ".".to_string(),
                    "-c".to_string(),
                    "user.name=review".to_string(),
                    "checkout".to_string(),
                    "other".to_string(),
                ],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo).with_mode(ModeOverlay {
                write_allowed: false,
                command_allowed: true,
                network_allowed: false,
            }),
        );
        assert_eq!(decision.decision, Decision::Deny);
        assert_eq!(
            decision.reasons[0].code,
            "policy.git-shell-write-denied-by-mode"
        );
    }

    /// `git push origin +main` force-pushes, and `git push -d origin x` deletes
    /// a branch, but neither spelling was matched: the force check looked only
    /// for `-f`/`--force*`, and the delete check accepted `-d` for `git branch`
    /// while requiring `--delete` for `git push`.
    ///
    /// Both fell through to the generic reusable-approval path, so a single
    /// approved `git push *` pattern would auto-approve force pushes and branch
    /// deletions from then on. Narrow the matches back and this fails.
    #[test]
    fn refspec_and_short_flag_spellings_do_not_escape_git_push_policy() {
        let mut merged = MergedPolicy::builtin_defaults();
        merged.git_force_push = ApprovalAction::Deny;
        merged.git_delete_branch = ApprovalAction::Deny;
        let engine = PolicyEngine::from_merged(merged);
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();

        let push = |args: Vec<&str>| {
            engine.evaluate(
                &ProposedAction::ExecuteCommand {
                    program: "git".to_string(),
                    args: args.into_iter().map(str::to_string).collect(),
                    environment: Vec::new(),
                    cwd: None,
                },
                &ctx(&repo, &repo),
            )
        };

        // A leading `+` on the refspec is a force push.
        assert_eq!(
            push(vec!["push", "origin", "+main"]).decision,
            Decision::Deny,
            "`git push origin +main` is a force push"
        );
        assert_eq!(
            push(vec!["push", "origin", "+refs/heads/main:refs/heads/main"]).decision,
            Decision::Deny,
            "a fully spelled forcing refspec is still a force push"
        );
        // `-d` is git's shorthand for `--delete`.
        assert_eq!(
            push(vec!["push", "-d", "origin", "feature"]).decision,
            Decision::Deny,
            "`git push -d` deletes a branch"
        );

        // The ordinary push is untouched: this must not deny everything.
        assert_ne!(
            push(vec!["push", "origin", "main"]).decision,
            Decision::Deny,
            "a plain push must not be caught by the widened match"
        );
    }

    #[test]
    fn git_global_options_cannot_hide_network_or_force_push_policy() {
        let mut merged = MergedPolicy::builtin_defaults();
        merged.git_force_push = ApprovalAction::Deny;
        let engine = PolicyEngine::from_merged(merged);
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();

        let read_only = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "git".to_string(),
                args: vec!["-C.".to_string(), "push".to_string(), "--force".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo).with_mode(ModeOverlay {
                write_allowed: false,
                command_allowed: true,
                network_allowed: false,
            }),
        );
        assert_eq!(read_only.decision, Decision::Deny);
        assert_eq!(
            read_only.reasons[0].code,
            "policy.git-shell-network-denied-by-mode"
        );

        let build = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "git".to_string(),
                args: vec![
                    "--git-dir".to_string(),
                    ".git".to_string(),
                    "--work-tree".to_string(),
                    ".".to_string(),
                    "push".to_string(),
                    "--force-with-lease=main".to_string(),
                ],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(build.decision, Decision::Deny);
        assert_eq!(build.reasons[0].code, "policy.git-shell-denied");
    }

    #[test]
    fn unknown_git_global_options_fail_closed() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "git".to_string(),
                args: vec!["--future-option".to_string(), "checkout".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::Deny);
        assert_eq!(
            decision.reasons[0].code,
            "policy.git-global-option-unsupported"
        );
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

    // --- PR B: MCP tool calls ---

    fn mcp_action(server: &str) -> ProposedAction {
        ProposedAction::McpToolCall {
            server: server.to_string(),
            tool: "create_issue".to_string(),
            summary: format!("{server}.create_issue(…)"),
            args: "{\"title\":\"x\"}".to_string(),
        }
    }

    /// The builtin default: no `[mcp]` config → every MCP call requires approval,
    /// and the minted grant is the marker `McpToolCall` capability naming the
    /// server (no path/command/network scope — the bridge executes the call).
    #[test]
    fn mcp_tool_call_defaults_to_require_approval() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(&mcp_action("github"), &ctx(&repo, &repo));
        assert_eq!(decision.decision, Decision::RequireApproval);
        assert_eq!(decision.reasons[0].code, "policy.mcp-requires-approval");
        assert!(matches!(
            decision.capability_grant.unwrap().capability,
            Capability::McpToolCall { ref server } if server == "github"
        ));
    }

    /// An operator allow-listed server runs ungated; a denied server never runs.
    #[test]
    fn mcp_tool_call_follows_per_server_disposition() {
        let mut merged = MergedPolicy::builtin_defaults();
        merged
            .mcp_servers
            .insert("github".to_string(), ApprovalAction::Allow);
        merged
            .mcp_servers
            .insert("experimental".to_string(), ApprovalAction::Deny);
        let engine = PolicyEngine::from_merged(merged);
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();

        let allowed = engine.evaluate(&mcp_action("github"), &ctx(&repo, &repo));
        assert_eq!(allowed.decision, Decision::Allow);
        assert_eq!(allowed.reasons[0].code, "policy.mcp-allowed");

        let denied = engine.evaluate(&mcp_action("experimental"), &ctx(&repo, &repo));
        assert_eq!(denied.decision, Decision::Deny);
        assert_eq!(denied.reasons[0].code, "policy.mcp-denied");
    }

    /// An MCP call is an external effect: a read-only mode forbids it outright,
    /// even for an allow-listed server (mode overlays win over dispositions).
    #[test]
    fn mcp_tool_call_denied_by_mode() {
        let mut merged = MergedPolicy::builtin_defaults();
        merged
            .mcp_servers
            .insert("github".to_string(), ApprovalAction::Allow);
        let engine = PolicyEngine::from_merged(merged);
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &mcp_action("github"),
            &ctx(&repo, &repo).with_mode(ModeOverlay::read_only()),
        );
        assert_eq!(decision.decision, Decision::Deny);
        assert_eq!(decision.reasons[0].code, "policy.mcp-denied-by-mode");
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

    /// `memory.remember` (smarter-memory M2): its only effect is a note on the
    /// run's own ledger, so it is always `Allow`ed regardless of mode/scope —
    /// mirroring the blackboard tools' unconditional allow.
    #[test]
    fn record_memory_is_always_allowed() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::RecordMemory,
            &ctx(&repo, &repo).with_mode(ModeOverlay::read_only()),
        );
        assert_eq!(decision.decision, Decision::Allow);
        assert_eq!(decision.reasons[0].code, "policy.record-memory-allowed");
        assert!(decision.capability_grant.is_none());
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

    // --- PF4: trust routing in `PolicyEngine::load` ---

    /// End-to-end widen: a **trusted global** policy adding `pytest` to the
    /// shell allow-list must actually move `pytest` from `Deny` to
    /// `RequireApproval` once loaded through `PolicyEngine::load` — never to
    /// auto-run (the approval gate is untouched by widening).
    #[test]
    fn load_trusted_global_widens_pytest_to_require_approval() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("global-policy.toml"),
            "[shell]\nallowed_programs = [\"pytest\"]\n",
        )
        .unwrap();

        let engine = PolicyEngine::load(None, Some(&dir.path().join("global-policy.toml")))
            .expect("load a well-formed trusted global policy");
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "pytest".to_string(),
                args: Vec::new(),
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(
            decision.decision,
            Decision::RequireApproval,
            "a trusted global policy must be able to widen the allow-list to pytest"
        );

        // Never auto-run: the same widened command still requires approval,
        // not Allow.
        assert_ne!(decision.decision, Decision::Allow);
    }

    /// The security-critical contrast: the SAME `pytest` addition through the
    /// **untrusted repo-local** layer must NOT take effect — `pytest` stays
    /// `Deny`d. Proves origin, not content, decides trust.
    #[test]
    fn load_untrusted_repo_cannot_widen_pytest() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("repo-policy.toml"),
            "[shell]\nallowed_programs = [\"cargo\", \"pytest\"]\n",
        )
        .unwrap();

        let engine = PolicyEngine::load(Some(&dir.path().join("repo-policy.toml")), None)
            .expect("load a well-formed untrusted repo policy");
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "pytest".to_string(),
                args: Vec::new(),
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(
            decision.decision,
            Decision::Deny,
            "a repo-local policy must never be able to widen the allow-list"
        );
        assert_eq!(decision.reasons[0].code, "policy.program-not-allowlisted");
    }

    /// A repo layer applied AFTER a widening global layer can still claw back
    /// what the global layer granted (narrow-only, applied last) — but never
    /// exceed it. Here the global widens to `pytest`, and the repo narrows
    /// the allow-list to just `cargo`, so `pytest` must not survive the merge.
    #[test]
    fn load_repo_layer_claws_back_what_global_widened() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("global-policy.toml");
        let repo_path = dir.path().join("repo-policy.toml");
        std::fs::write(&global_path, "[shell]\nallowed_programs = [\"pytest\"]\n").unwrap();
        std::fs::write(&repo_path, "[shell]\nallowed_programs = [\"cargo\"]\n").unwrap();

        let engine = PolicyEngine::load(Some(&repo_path), Some(&global_path))
            .expect("load both well-formed layers");
        let repo = dir.path().to_path_buf();

        let pytest_decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "pytest".to_string(),
                args: Vec::new(),
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(
            pytest_decision.decision,
            Decision::Deny,
            "the repo layer, applied last, must claw back the global's widen"
        );

        let cargo_decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "cargo".to_string(),
                args: Vec::new(),
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(cargo_decision.decision, Decision::RequireApproval);
    }

    /// A malformed file in EITHER layer must fail `load` with a
    /// `PolicyLoadError` — never a silently-defaulted engine. This is what
    /// lets a caller (the executor) map the error to a run failure instead of
    /// quietly falling back to weaker built-in defaults.
    #[test]
    fn load_malformed_global_policy_is_an_error_not_a_default() {
        let dir = tempdir().unwrap();
        let bad = dir.path().join("bad-global.toml");
        std::fs::write(&bad, "[shell]\nbogus_key = true\n").unwrap();

        let result = PolicyEngine::load(None, Some(&bad));
        assert!(
            result.is_err(),
            "a malformed global policy must be a load error, not a defaulted engine"
        );
    }

    /// `admitting_network` composes with a LOADED policy: the endpoint is
    /// admitted on top of whatever the file layers already granted, and
    /// admitting it alone still requires approval for a GitHub mutation
    /// (grants nothing on its own).
    #[test]
    fn admitting_network_composes_with_a_loaded_policy() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("global-policy.toml");
        std::fs::write(&global_path, "[shell]\nallowed_programs = [\"pytest\"]\n").unwrap();

        let engine = PolicyEngine::load(None, Some(&global_path))
            .expect("load a well-formed trusted global policy")
            .admitting_network([GITHUB_API_ENDPOINT.to_string()]);
        let repo = dir.path().to_path_buf();

        // The pytest widen from the file layer survived the post-load admit.
        let pytest_decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "pytest".to_string(),
                args: Vec::new(),
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(pytest_decision.decision, Decision::RequireApproval);

        let github_decision = engine.evaluate(
            &ProposedAction::GitHubMutation {
                repository: "octocat/hello-world".to_string(),
                summary: "create draft PR".to_string(),
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(github_decision.decision, Decision::RequireApproval);
    }

    #[test]
    fn ask_user_is_always_allowed_and_never_reusable() {
        let engine = PolicyEngine::with_defaults();
        let repo = std::path::PathBuf::from("/repo");
        let decision = engine.evaluate(
            &ProposedAction::AskUser {
                question_count: 1,
                headers: vec!["Database".to_string()],
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::Allow);
        assert!(!decision.approval_reusable);
        assert!(decision.capability_grant.is_none());
    }

    #[test]
    fn restore_checkpoint_always_requires_single_use_approval() {
        let engine = PolicyEngine::with_defaults();
        let repo = std::path::PathBuf::from("/repo");
        let decision = engine.evaluate(
            &ProposedAction::RestoreCheckpoint {
                run_id: "test-run".to_string(),
                ordinal: 1,
                worktree: "/repo/worktree".to_string(),
                commit: "abc1234".to_string(),
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::RequireApproval);
        assert!(!decision.approval_reusable);
        assert!(matches!(
            decision.capability_grant.map(|g| g.capability),
            Some(Capability::RestoreCheckpoint)
        ));
    }

    #[test]
    fn scanned_external_path_forces_fresh_approval() {
        let engine = PolicyEngine::with_defaults();
        let repo = std::path::PathBuf::from("/repo");
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "cat".to_string(),
                args: vec!["../../../etc/passwd".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::RequireApproval);
        assert!(!decision.approval_reusable);
        assert!(decision
            .reasons
            .iter()
            .any(|r| r.code == "policy.command-touches-external-paths"));
    }

    #[test]
    fn scanned_denied_path_denies() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("global-policy.toml");
        std::fs::write(
            &global_path,
            "[filesystem]\ndeny = [\"$WORKTREE/secret\"]\n",
        )
        .unwrap();

        let engine = PolicyEngine::load(None, Some(&global_path)).unwrap();
        let repo = dir.path().to_path_buf();
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "cat".to_string(),
                args: vec!["secret/id_rsa".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::Deny);
        assert!(decision
            .reasons
            .iter()
            .any(|r| r.code == "policy.command-path-denied"));
    }

    #[test]
    fn in_scope_paths_change_nothing() {
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let engine = PolicyEngine::with_defaults();
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "cat".to_string(),
                args: vec!["src/main.rs".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::RequireApproval);
        assert!(decision.approval_reusable);
        assert!(decision
            .reasons
            .iter()
            .any(|r| r.code == "policy.command-requires-approval"));
    }

    #[test]
    fn flags_and_glob_leading_args_are_skipped() {
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let engine = PolicyEngine::with_defaults();
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "cat".to_string(),
                args: vec!["-n".to_string(), "-v".to_string(), "*.sh".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::RequireApproval);
        assert!(decision.approval_reusable);
    }

    #[test]
    fn scanning_applies_only_to_path_scanned_programs() {
        let dir = tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let engine = PolicyEngine::with_defaults();
        // cargo is not in PATH_SCANNED_PROGRAMS
        let decision = engine.evaluate(
            &ProposedAction::ExecuteCommand {
                program: "cargo".to_string(),
                args: vec!["test".to_string(), "/tmp/out-of-scope".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            &ctx(&repo, &repo),
        );
        assert_eq!(decision.decision, Decision::RequireApproval);
        assert!(decision.approval_reusable);
    }

    #[test]
    fn plan_mode_allows_a_write_under_the_plan_root() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let worktree = std::fs::canonicalize(dir.path()).unwrap();
        let plan_root = worktree.join(".codypendent").join("plans");
        let plan_file = plan_root.join("plan.md");

        let plan_mode = ModeOverlay {
            write_allowed: false,
            command_allowed: true,
            network_allowed: false,
        };
        let eval_ctx = ctx(&worktree, &worktree)
            .with_mode(plan_mode)
            .with_plan_write_root(&plan_root)
            .with_write_target(&plan_file);

        let decision = engine.evaluate(
            &ProposedAction::WritePatch {
                patch: ArtifactId::new(),
            },
            &eval_ctx,
        );

        assert_eq!(decision.decision, Decision::Allow);
        assert_eq!(decision.reasons[0].code, "policy.plan-file-write-allowed");
        let grant = decision.capability_grant.expect("capability grant");
        match grant.capability {
            Capability::FileWrite(scope) => {
                assert!(scope.allows(&plan_file));
                assert!(!scope.allows(&worktree.join("src/main.rs")));
            }
            other => panic!("expected FileWrite capability, got {other:?}"),
        }
    }

    #[test]
    fn plan_mode_denies_a_write_outside_the_plan_root() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let worktree = std::fs::canonicalize(dir.path()).unwrap();
        let plan_root = worktree.join(".codypendent").join("plans");
        let outside_file = worktree.join("src").join("main.rs");

        let plan_mode = ModeOverlay {
            write_allowed: false,
            command_allowed: true,
            network_allowed: false,
        };
        let eval_ctx = ctx(&worktree, &worktree)
            .with_mode(plan_mode)
            .with_plan_write_root(&plan_root)
            .with_write_target(&outside_file);

        let decision = engine.evaluate(
            &ProposedAction::WritePatch {
                patch: ArtifactId::new(),
            },
            &eval_ctx,
        );

        assert_eq!(decision.decision, Decision::Deny);
        assert_eq!(decision.reasons[0].code, "policy.write-denied-by-mode");
    }

    #[test]
    fn plan_mode_denies_a_write_with_no_target() {
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let worktree = std::fs::canonicalize(dir.path()).unwrap();
        let plan_root = worktree.join(".codypendent").join("plans");

        let plan_mode = ModeOverlay {
            write_allowed: false,
            command_allowed: true,
            network_allowed: false,
        };
        // Multi-file patch case: plan_write_root is set, but write_target is None
        let eval_ctx = ctx(&worktree, &worktree)
            .with_mode(plan_mode)
            .with_plan_write_root(&plan_root);

        let decision = engine.evaluate(
            &ProposedAction::WritePatch {
                patch: ArtifactId::new(),
            },
            &eval_ctx,
        );

        assert_eq!(decision.decision, Decision::Deny);
        assert_eq!(decision.reasons[0].code, "policy.write-denied-by-mode");
    }

    #[test]
    fn plan_transition_is_always_allowed_and_not_reusable() {
        use codypendent_protocol::AgentMode;
        let engine = PolicyEngine::with_defaults();
        let dir = tempdir().unwrap();
        let worktree = std::fs::canonicalize(dir.path()).unwrap();

        for target in [AgentMode::Plan, AgentMode::Build] {
            let decision = engine.evaluate(
                &ProposedAction::PlanTransition { target },
                &ctx(&worktree, &worktree).with_mode(ModeOverlay::read_only()),
            );
            assert_eq!(decision.decision, Decision::Allow);
            assert_eq!(decision.reasons[0].code, "policy.plan-transition-allowed");
            assert!(!decision.approval_reusable);
            assert!(decision.capability_grant.is_none());
        }
    }
}
