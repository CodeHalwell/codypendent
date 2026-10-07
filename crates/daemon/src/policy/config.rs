//! Policy files, the layered merge, and the built-in defaults (STEP 1.5).
//!
//! Three layers stack, broadest authority first:
//!
//! 1. built-in [`MergedPolicy::builtin_defaults`] (the System baseline);
//! 2. `<config_dir>/codypendent/policy.toml` (the User layer);
//! 3. `<repo>/.codypendent/policy.toml` (the Repository layer, narrowest).
//!
//! Each file layer is a [`RawPolicy`] applied over the accumulating
//! [`MergedPolicy`] by either [`MergedPolicy::apply_untrusted_overlay`] (a
//! repo-local file — may only *restrict* a security scope, never widen it:
//! allowed roots and allow-lists intersect; deny lists union; approval
//! requirements ratchet toward the more restrictive value) or
//! [`MergedPolicy::apply_trusted_overlay`] (the user's own global config —
//! may *widen or narrow* the shell allow-list, network allow-list, and
//! `fs_read` scope, and may relax git/network approval dispositions;
//! `fs_write` stays narrow-only even here, since worktree confinement is the
//! floor no file may widen). Preferences that carry no security weight are
//! simply overridden.
//!
//! Every section that appears in `docs/specs/policy.toml` is modeled with
//! `#[serde(deny_unknown_fields)]`, so an unknown key is a load *error*, not a
//! silently-ignored warning (guide RULE 4).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::scope::NetworkDefault;

/// An approval disposition shared by the `[git]` and `[plugins]` sections.
/// Ordered from least to most restrictive by [`ApprovalAction::rank`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalAction {
    /// Permit without prompting.
    Allow,
    /// Permit once a human approves this occurrence.
    Approval,
    /// Always require a fresh approval; a prior approval never carries over.
    AlwaysApproval,
    /// Never permit.
    Deny,
}

impl ApprovalAction {
    fn rank(self) -> u8 {
        match self {
            ApprovalAction::Allow => 0,
            ApprovalAction::Approval => 1,
            ApprovalAction::AlwaysApproval => 2,
            ApprovalAction::Deny => 3,
        }
    }

    /// The stricter of two dispositions.
    fn more_restrictive(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
}

/// The effective, fully-resolved policy after the three layers are merged.
/// Path fields still hold unexpanded `$REPOSITORY`/`$WORKTREE`/`$HOME` strings;
/// they are expanded and canonicalized per evaluation (variables resolve at
/// evaluation time). Serialized deterministically to derive a stable
/// `PolicyVersion` hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergedPolicy {
    pub schema_version: u32,
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    pub fs_deny: Vec<String>,
    /// The `fs_read` / `fs_write` roots that an UNTRUSTED layer (a repo-local
    /// `.codypendent/policy.toml`) introduced by narrowing a broader root to a
    /// subpath. They are the only roots whose location the repository itself
    /// controls — it can commit a symlink at `$WORKTREE/link` and name that path
    /// — so evaluation re-checks that each still lies inside its own anchor after
    /// symlinks are resolved (`build_path_scope`). Roots from trusted layers are
    /// not listed: a user may legitimately point `$HOME/code` at another disk.
    /// Omitted from the serialized form when empty, so a policy with no such
    /// root derives the same `PolicyVersion` it always did.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub untrusted_fs_roots: Vec<String>,
    pub shell_allowed_programs: Vec<String>,
    pub shell_interpreter_requires_approval: bool,
    pub shell_maximum_seconds: u64,
    pub network_allow: Vec<String>,
    pub network_default: NetworkDefault,
    pub git_commit: ApprovalAction,
    pub git_push: ApprovalAction,
    pub git_force_push: ApprovalAction,
    pub git_delete_branch: ApprovalAction,
    /// The disposition for MCP tool calls to servers with no explicit
    /// `[mcp.servers]` entry (PR B). Builtin default: `Approval`.
    pub mcp_default: ApprovalAction,
    /// Per-server MCP dispositions. A `BTreeMap` so the derived `PolicyVersion`
    /// hash is deterministic.
    pub mcp_servers: std::collections::BTreeMap<String, ApprovalAction>,
}

impl MergedPolicy {
    /// The built-in defaults used when no policy file exists (guide item 6):
    /// read = repository; write = worktree only; shell allow-list requiring
    /// approval; network denied; git commit/push require approval. The deny
    /// list guards `.git` and common secret stores so deny-precedence holds
    /// even with no file present.
    pub fn builtin_defaults() -> Self {
        Self {
            schema_version: 1,
            fs_read: vec!["$REPOSITORY".to_string()],
            fs_write: vec!["$WORKTREE".to_string()],
            fs_deny: vec![
                "$REPOSITORY/.git".to_string(),
                "$WORKTREE/.git".to_string(),
                "$HOME/.ssh".to_string(),
                "$HOME/.config".to_string(),
            ],
            untrusted_fs_roots: Vec::new(),
            shell_allowed_programs: vec![
                // Original Rust baseline.
                "cargo".to_string(),
                "git".to_string(),
                "rg".to_string(),
                "rustfmt".to_string(),
                // Curated, language-agnostic, read-only exploration set (agent &
                // tool fixes spec, FIX 2a): without these, basic exploration of a
                // non-Rust repository denied `ls`/`find` outright and the agent
                // fell back to repeated file reads. `find` is deliberately
                // excluded (`-delete`/`-exec`/`-execdir`/`-ok` can remove files or
                // run arbitrary programs, bypassing this allow-list), as is every
                // interpreter/code-execution multiplexer. Allow-listing a program
                // here does not auto-run it — every allow-listed command still
                // requires approval (see `eval_command`).
                "ls".to_string(),
                "cat".to_string(),
                "head".to_string(),
                "tail".to_string(),
                "wc".to_string(),
                "grep".to_string(),
                "tree".to_string(),
                "pwd".to_string(),
                "file".to_string(),
                "which".to_string(),
                "sort".to_string(),
                "uniq".to_string(),
            ],
            shell_interpreter_requires_approval: true,
            shell_maximum_seconds: 900,
            network_allow: Vec::new(),
            network_default: NetworkDefault::Deny,
            git_commit: ApprovalAction::Approval,
            git_push: ApprovalAction::Approval,
            git_force_push: ApprovalAction::Deny,
            git_delete_branch: ApprovalAction::AlwaysApproval,
            mcp_default: ApprovalAction::Approval,
            mcp_servers: std::collections::BTreeMap::new(),
        }
    }

    /// Record, in [`untrusted_fs_roots`](Self::untrusted_fs_roots), each root in
    /// `narrowed` that is not one of the `broader` roots it was derived from —
    /// i.e. a subpath the untrusted layer chose.
    fn note_overlay_roots(&mut self, broader: &[String], narrowed: &[String]) {
        let broader: Vec<String> = broader.iter().filter_map(|r| normalize_raw(r)).collect();
        for root in narrowed {
            if !broader.contains(root) && !self.untrusted_fs_roots.contains(root) {
                self.untrusted_fs_roots.push(root.clone());
            }
        }
    }

    /// Apply a narrower file layer over this policy, enforcing the merge
    /// invariant. Only fields the overlay sets are touched; each is narrowed,
    /// never widened. Used for an **untrusted** source (a repo-local
    /// `.codypendent/policy.toml`): the file may only claw back authority the
    /// broader layers already granted, never add a program, root, endpoint,
    /// or relax an approval disposition.
    pub fn apply_untrusted_overlay(&mut self, raw: &RawPolicy) {
        if let Some(schema_version) = raw.schema_version {
            self.schema_version = schema_version;
        }
        if let Some(fs) = &raw.filesystem {
            if let Some(read) = &fs.read {
                let narrowed = intersect_roots(&self.fs_read, read);
                self.note_overlay_roots(&self.fs_read.clone(), &narrowed);
                self.fs_read = narrowed;
            }
            if let Some(write) = &fs.write {
                let narrowed = intersect_roots(&self.fs_write, write);
                self.note_overlay_roots(&self.fs_write.clone(), &narrowed);
                self.fs_write = narrowed;
            }
            if let Some(deny) = &fs.deny {
                // Deny accumulates: a narrower layer can add denials but never
                // remove one a broader layer imposed.
                union_in_place(&mut self.fs_deny, deny);
            }
        }
        if let Some(shell) = &raw.shell {
            if let Some(programs) = &shell.allowed_programs {
                self.shell_allowed_programs =
                    intersect_exact(&self.shell_allowed_programs, programs);
            }
            if let Some(requires) = shell.shell_interpreter_requires_approval {
                // Requiring approval is stricter; once required it stays required.
                self.shell_interpreter_requires_approval |= requires;
            }
            if let Some(seconds) = shell.maximum_seconds {
                self.shell_maximum_seconds = self.shell_maximum_seconds.min(seconds);
            }
        }
        if let Some(network) = &raw.network {
            if let Some(allow) = &network.allow {
                self.network_allow = intersect_exact(&self.network_allow, allow);
            }
            if let Some(default) = network.default {
                // Deny is stricter than Allow.
                if matches!(default, NetworkDefault::Deny) {
                    self.network_default = NetworkDefault::Deny;
                }
            }
        }
        if let Some(git) = &raw.git {
            if let Some(commit) = git.commit {
                self.git_commit = self.git_commit.more_restrictive(commit);
            }
            if let Some(push) = git.push {
                self.git_push = self.git_push.more_restrictive(push);
            }
            if let Some(force_push) = git.force_push {
                self.git_force_push = self.git_force_push.more_restrictive(force_push);
            }
            if let Some(delete_branch) = git.delete_branch {
                self.git_delete_branch = self.git_delete_branch.more_restrictive(delete_branch);
            }
        }
        if let Some(mcp) = &raw.mcp {
            if let Some(default) = mcp.default {
                self.mcp_default = self.mcp_default.more_restrictive(default);
            }
            if let Some(servers) = &mcp.servers {
                for (name, action) in servers {
                    // Narrow-only: the server's *effective* disposition (its
                    // explicit entry, else the default) may only be raised — a
                    // repo layer can never relax what the broader layers set.
                    let effective = self
                        .mcp_servers
                        .get(name)
                        .copied()
                        .unwrap_or(self.mcp_default);
                    self.mcp_servers
                        .insert(name.clone(), effective.more_restrictive(*action));
                }
            }
        }
        // `scope`, `data`, `plugins`, and `memory` are parsed (and validated for
        // unknown keys) but not enforced in Phase 1.
    }

    /// Apply a **trusted** file layer over this policy — the user's own
    /// global config, never a repo-local file. Unlike
    /// [`Self::apply_untrusted_overlay`], this path may *widen* the shell
    /// allow-list, network allow-list, and `fs_read` scope, and may *relax*
    /// git/network approval dispositions (the overlay value replaces the
    /// accumulated one in either direction). `fs_write` is the one exception:
    /// it stays **narrow-only** even from a trusted source, because worktree
    /// confinement (`fs_write = $WORKTREE`) is the floor that guarantees a
    /// run cannot write outside its isolated worktree — no config file, not
    /// even the user's own, may widen it (Decision 2a).
    ///
    /// Widening the shell/network allow-lists only changes which programs or
    /// endpoints are *considered* — it never bypasses the approval gate.
    /// Every allow-listed program still requires a human approval at
    /// evaluation time (`eval_command`); widening can move a program from
    /// `Deny` to `RequireApproval`, never to auto-run.
    ///
    /// Called by [`super::PolicyEngine::load`] for the global/config-dir
    /// layer (PF4) — the untrusted repo-local layer stays on
    /// [`Self::apply_untrusted_overlay`].
    pub fn apply_trusted_overlay(&mut self, raw: &RawPolicy) {
        if let Some(schema_version) = raw.schema_version {
            self.schema_version = schema_version;
        }
        if let Some(fs) = &raw.filesystem {
            if let Some(read) = &fs.read {
                // Widen: a trusted global may extend read scope.
                self.fs_read = union_roots(&self.fs_read, read);
            }
            if let Some(write) = &fs.write {
                // Narrow-only floor (Decision 2a): even the trusted global
                // config cannot widen where writes may land.
                self.fs_write = intersect_roots(&self.fs_write, write);
            }
            if let Some(deny) = &fs.deny {
                // Deny accumulates regardless of trust: adding a denial only
                // tightens, so it is always safe to union.
                union_in_place(&mut self.fs_deny, deny);
            }
        }
        if let Some(shell) = &raw.shell {
            if let Some(programs) = &shell.allowed_programs {
                // Widen: the trusted global may add programs (e.g. `pytest`)
                // while keeping the built-in/broader set (union, not
                // replace). Adding a program here only makes it eligible for
                // `RequireApproval`, never for auto-run.
                union_in_place(&mut self.shell_allowed_programs, programs);
            }
            if let Some(requires) = shell.shell_interpreter_requires_approval {
                // Either direction: a trusted global may relax or tighten.
                self.shell_interpreter_requires_approval = requires;
            }
            if let Some(seconds) = shell.maximum_seconds {
                // Either direction: a trusted global may relax or tighten.
                self.shell_maximum_seconds = seconds;
            }
        }
        if let Some(network) = &raw.network {
            if let Some(allow) = &network.allow {
                // Widen: trusted global may add network endpoints.
                union_in_place(&mut self.network_allow, allow);
            }
            if let Some(default) = network.default {
                // Either direction: a trusted global may relax network
                // default to `Allow`.
                self.network_default = default;
            }
        }
        if let Some(git) = &raw.git {
            if let Some(commit) = git.commit {
                // Either direction: a trusted global may relax approval.
                self.git_commit = commit;
            }
            if let Some(push) = git.push {
                self.git_push = push;
            }
            if let Some(force_push) = git.force_push {
                self.git_force_push = force_push;
            }
            if let Some(delete_branch) = git.delete_branch {
                self.git_delete_branch = delete_branch;
            }
        }
        if let Some(mcp) = &raw.mcp {
            if let Some(default) = mcp.default {
                // Either direction: a trusted global may relax or tighten.
                self.mcp_default = default;
            }
            if let Some(servers) = &mcp.servers {
                // Either direction, per server — entries replace, like `[git]`.
                self.mcp_servers.extend(servers.clone());
            }
        }
        // `scope`, `data`, `plugins`, and `memory` are parsed (and validated for
        // unknown keys) but not enforced in Phase 1.
    }
}

/// Region intersection of two allowed-root lists. Both `base` and `overlay`
/// are first lexically normalized ([`normalize_raw`]) so containment is
/// decided on collapsed, anchor-safe strings, never on a raw `..`-laden one
/// (D3): a root that fails to normalize (escapes its anchor, or carries no
/// recognized anchor) is dropped before any comparison happens, from either
/// side. A root from `overlay` is kept only where it lies within a `base`
/// root (the narrower region wins); where a `base` root lies within an
/// `overlay` root, the `base` root is kept (still no widening). Disjoint
/// pairs contribute nothing. The result is therefore always a subset of the
/// region `base` already permitted — a narrower layer can never add a root
/// the broader layer did not allow, and an escaping root can never survive.
fn intersect_roots(base: &[String], overlay: &[String]) -> Vec<String> {
    let base: Vec<String> = base.iter().filter_map(|root| normalize_raw(root)).collect();
    let overlay: Vec<String> = overlay
        .iter()
        .filter_map(|root| normalize_raw(root))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for narrow in &overlay {
        for broad in &base {
            let kept = if raw_within(narrow, broad) {
                Some(narrow)
            } else if raw_within(broad, narrow) {
                Some(broad)
            } else {
                None
            };
            if let Some(root) = kept {
                if !out.iter().any(|existing| existing == root) {
                    out.push(root.clone());
                }
            }
        }
    }
    out
}

/// Set intersection preserving `base` order (used for exact-match lists such as
/// shell programs and network destinations).
fn intersect_exact(base: &[String], overlay: &[String]) -> Vec<String> {
    base.iter()
        .filter(|item| overlay.iter().any(|o| o == *item))
        .cloned()
        .collect()
}

/// Region union of two allowed-root lists — the widening dual of
/// [`intersect_roots`]. Every `base` root is kept (normalized; see
/// [`normalize_raw`]); an `overlay` root is normalized and, unless it fails to
/// normalize (D3: escapes its anchor, or carries no recognized anchor — such
/// a root is dropped, fail-closed, even on this trusted widen path) or
/// already lies within a root already in the result, appended. Used only by
/// the **trusted** widen path (`apply_trusted_overlay`) for `fs_read`;
/// `fs_write` never calls this — it stays on `intersect_roots` even for a
/// trusted source.
fn union_roots(base: &[String], overlay: &[String]) -> Vec<String> {
    let mut out: Vec<String> = base.iter().filter_map(|root| normalize_raw(root)).collect();
    for candidate in overlay {
        let Some(candidate) = normalize_raw(candidate) else {
            // Escapes its anchor (or has none): unsafe, drop fail-closed —
            // even a trusted source's widen may never admit this.
            continue;
        };
        let already_covered = out.iter().any(|existing| raw_within(&candidate, existing));
        if !already_covered {
            out.push(candidate);
        }
    }
    out
}

/// Append entries of `extra` to `base` that are not already present.
fn union_in_place(base: &mut Vec<String>, extra: &[String]) {
    for item in extra {
        if !base.iter().any(|existing| existing == item) {
            base.push(item.clone());
        }
    }
}

/// Component-wise containment on unexpanded root strings: `inner` is `outer` or
/// lies under it. Both operands are lexically normalized first ([`normalize_raw`]
/// — D3), so containment is decided on collapsed, anchor-safe strings rather
/// than a raw string that could still carry an un-collapsed `..`. If either
/// operand fails to normalize (escapes its anchor, or has none), containment
/// is refused (`false`): merge-time containment must never be decided on an
/// unsafe string. `$REPOSITORY`, `$WORKTREE`, and `$HOME` are treated as
/// opaque leading anchors, so `$WORKTREE/src` is within `$WORKTREE` while
/// `$WORKTREE/../etc` (an escape) normalizes to `None` and is never "within"
/// anything.
fn raw_within(inner: &str, outer: &str) -> bool {
    let (Some(inner), Some(outer)) = (normalize_raw(inner), normalize_raw(outer)) else {
        return false;
    };
    let inner = Path::new(&inner);
    let outer = Path::new(&outer);
    inner == outer || inner.starts_with(outer)
}

/// The anchor tokens a raw policy root string may begin with. Each is an
/// **opaque placeholder** here — never resolved or filesystem-canonicalized
/// at merge time (that happens later, at evaluation time, once the run's
/// actual repository/worktree/home paths are known). This normalizer only
/// reasons about the anchor as an immovable leading component that a `..`
/// may never pop past.
const ROOT_ANCHORS: [&str; 3] = ["$REPOSITORY", "$WORKTREE", "$HOME"];

/// D3 fix: lexically collapse `.`/`..` in a raw (unexpanded) policy root
/// string, treating a leading anchor token ([`ROOT_ANCHORS`]) as an opaque,
/// immovable component.
///
/// Returns `None` — the root is unsafe and must be **dropped** (fail-closed)
/// from any merged scope — when:
/// - the string does not begin with a recognized anchor token (a root this
///   merge-time check cannot reason about at all), or
/// - a `..` component would pop above the anchor itself (an escape), e.g.
///   `$WORKTREE/../etc` pops past `$WORKTREE` and is rejected.
///
/// Otherwise returns `Some(normalized)`: the anchor followed by the collapsed
/// subpath (`.` dropped, safe `..` cancelling a prior real component), e.g.
/// `$WORKTREE/sub/../other` → `Some("$WORKTREE/other")`.
///
/// This is purely a **lexical** operation on the anchor+subpath string — the
/// anchor is never resolved to a filesystem path here. Real resolution
/// (`$WORKTREE` → an actual directory, followed by `std::fs::canonicalize`)
/// happens only at evaluation time (`build_path_scope` / `canonicalize_lenient`
/// in `scope.rs`), which is already correct; this closes the earlier
/// merge-time hole where an escaping root could still survive `intersect_roots`
/// on its un-collapsed string and only later canonicalize outside the anchor.
fn normalize_raw(raw: &str) -> Option<String> {
    let mut segments = raw.split('/').filter(|segment| !segment.is_empty());
    let anchor = segments.next()?;
    if !ROOT_ANCHORS.contains(&anchor) {
        return None;
    }
    let mut stack: Vec<&str> = Vec::new();
    for segment in segments {
        match segment {
            "." => {}
            ".." => {
                // Would pop above the anchor itself if the stack is already
                // empty — an escape.
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    let mut normalized = anchor.to_string();
    for segment in stack {
        normalized.push('/');
        normalized.push_str(segment);
    }
    Some(normalized)
}

/// One policy file, as parsed. Every section is optional; a layer that omits a
/// section leaves the accumulated value untouched. `deny_unknown_fields` makes a
/// stray key a hard error at every level.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPolicy {
    #[serde(default)]
    pub schema_version: Option<u32>,
    // `scope`, `data`, `plugins`, and `memory` are modeled so the spec file
    // parses and so unknown keys inside them are rejected, but they carry no
    // Phase 1 enforcement — hence parsed-but-unread.
    #[serde(default)]
    #[allow(dead_code)]
    pub scope: Option<RawScope>,
    #[serde(default)]
    #[allow(dead_code)]
    pub data: Option<RawData>,
    #[serde(default)]
    pub filesystem: Option<RawFilesystem>,
    #[serde(default)]
    pub shell: Option<RawShell>,
    #[serde(default)]
    pub network: Option<RawNetwork>,
    #[serde(default)]
    pub git: Option<RawGit>,
    #[serde(default)]
    pub mcp: Option<RawMcp>,
    #[serde(default)]
    #[allow(dead_code)]
    pub plugins: Option<RawPlugins>,
    #[serde(default)]
    #[allow(dead_code)]
    pub memory: Option<RawMemory>,
}

impl RawPolicy {
    /// Parse a policy file's contents. Unknown keys fail here.
    pub fn parse(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // parsed for validation; not enforced in Phase 1
pub struct RawScope {
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // parsed for validation; not enforced in Phase 1
pub struct RawData {
    #[serde(default)]
    pub classification: Option<String>,
    #[serde(default)]
    pub remote_models_allowed: Option<Vec<String>>,
    #[serde(default)]
    pub local_models_allowed: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawFilesystem {
    #[serde(default)]
    pub read: Option<Vec<String>>,
    #[serde(default)]
    pub write: Option<Vec<String>>,
    #[serde(default)]
    pub deny: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawShell {
    #[serde(default)]
    pub allowed_programs: Option<Vec<String>>,
    #[serde(default)]
    pub shell_interpreter_requires_approval: Option<bool>,
    #[serde(default)]
    pub maximum_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawNetwork {
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub default: Option<NetworkDefault>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawGit {
    #[serde(default)]
    pub commit: Option<ApprovalAction>,
    #[serde(default)]
    pub push: Option<ApprovalAction>,
    #[serde(default)]
    pub force_push: Option<ApprovalAction>,
    #[serde(default)]
    pub delete_branch: Option<ApprovalAction>,
}

/// The `[mcp]` section (PR B — MCP client): the disposition for tool calls to
/// operator-declared MCP servers. `default` covers servers with no explicit
/// entry; `[mcp.servers]` overrides per server. The trusted global layer may
/// relax either direction; the repo-local layer narrows only.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMcp {
    #[serde(default)]
    pub default: Option<ApprovalAction>,
    #[serde(default)]
    pub servers: Option<std::collections::BTreeMap<String, ApprovalAction>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // parsed for validation; not enforced in Phase 1
pub struct RawPlugins {
    #[serde(default)]
    pub unsigned: Option<ApprovalAction>,
    #[serde(default)]
    pub native_process: Option<ApprovalAction>,
    #[serde(default)]
    pub permission_expansion: Option<ApprovalAction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // parsed for validation; not enforced in Phase 1
pub struct RawMemory {
    #[serde(default)]
    pub cross_repository: Option<bool>,
    #[serde(default)]
    pub retain_days: Option<i64>,
    #[serde(default)]
    pub secrets: Option<String>,
}

/// A failure loading or parsing a policy file.
#[derive(Debug, thiserror::Error)]
pub enum PolicyLoadError {
    /// The file existed but could not be read.
    #[error("failed to read policy file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file's contents were not valid policy TOML (includes unknown keys).
    #[error("failed to parse policy file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Read and parse a policy file. `Ok(None)` if the path does not exist (so a
/// caller may pass conventional locations without pre-checking); a parse error
/// — including an unknown key — is returned as [`PolicyLoadError::Parse`].
pub fn load_layer(path: &Path) -> Result<Option<RawPolicy>, PolicyLoadError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PolicyLoadError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let raw = RawPolicy::parse(&contents).map_err(|source| PolicyLoadError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_roots_never_widens() {
        // A narrower layer that lists a root outside the broader region does
        // not gain it; only the in-region roots survive.
        let base = vec!["$WORKTREE".to_string()];
        let overlay = vec!["$WORKTREE/src".to_string(), "/tmp/evil".to_string()];
        let merged = intersect_roots(&base, &overlay);
        assert_eq!(merged, vec!["$WORKTREE/src".to_string()]);
        assert!(!merged.iter().any(|r| r == "/tmp/evil"));
    }

    #[test]
    fn intersect_roots_keeps_broader_when_overlay_is_wider() {
        let base = vec!["$REPOSITORY".to_string()];
        let overlay = vec!["$REPOSITORY".to_string(), "/etc".to_string()];
        let merged = intersect_roots(&base, &overlay);
        assert_eq!(merged, vec!["$REPOSITORY".to_string()]);
    }

    /// Property: for any broader/narrower pair drawn from a small alphabet, every
    /// root in the merged result lies within some broader root — the effective
    /// scope is always a subset of the broader region, never a union.
    #[test]
    fn intersect_roots_result_is_always_within_base() {
        let alphabet = [
            "$WORKTREE",
            "$WORKTREE/src",
            "$WORKTREE/src/lib",
            "$REPOSITORY",
            "/tmp/evil",
            "/etc",
        ];
        // Exhaustively enumerate base and overlay as 2-element selections.
        for &b0 in &alphabet {
            for &b1 in &alphabet {
                let base = vec![b0.to_string(), b1.to_string()];
                for &o0 in &alphabet {
                    for &o1 in &alphabet {
                        let overlay = vec![o0.to_string(), o1.to_string()];
                        let merged = intersect_roots(&base, &overlay);
                        for root in &merged {
                            assert!(
                                base.iter().any(|b| raw_within(root, b)),
                                "merged root {root} escaped base {base:?} (overlay {overlay:?})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn deny_accumulates_and_dedups() {
        let mut merged = MergedPolicy::builtin_defaults();
        let raw =
            RawPolicy::parse("[filesystem]\ndeny = [\"$WORKTREE/secrets\", \"$WORKTREE/.git\"]")
                .unwrap();
        let before = merged.fs_deny.len();
        merged.apply_untrusted_overlay(&raw);
        // New entry added once; the already-present `$WORKTREE/.git` not duped.
        assert!(merged.fs_deny.iter().any(|d| d == "$WORKTREE/secrets"));
        assert_eq!(
            merged
                .fs_deny
                .iter()
                .filter(|d| *d == "$WORKTREE/.git")
                .count(),
            1
        );
        assert_eq!(merged.fs_deny.len(), before + 1);
    }

    #[test]
    fn untrusted_overlay_records_only_the_roots_it_introduced() {
        // Re-stating a broader layer's own root introduces nothing new...
        let mut merged = MergedPolicy::builtin_defaults();
        merged.apply_untrusted_overlay(
            &RawPolicy::parse("[filesystem]\nwrite = [\"$WORKTREE\"]").unwrap(),
        );
        assert!(merged.untrusted_fs_roots.is_empty());

        // ...but a subpath the repository chose is the repository's doing.
        merged.apply_untrusted_overlay(
            &RawPolicy::parse("[filesystem]\nwrite = [\"$WORKTREE/src\"]").unwrap(),
        );
        assert_eq!(merged.untrusted_fs_roots, vec!["$WORKTREE/src".to_string()]);
        assert_eq!(merged.fs_write, vec!["$WORKTREE/src".to_string()]);
    }

    #[test]
    fn a_policy_with_no_untrusted_roots_keeps_its_serialized_form() {
        // `PolicyVersion` is a hash of this serialization; adding the provenance
        // list must not change the version of any policy that has none.
        let json = serde_json::to_string(&MergedPolicy::builtin_defaults()).unwrap();
        assert!(!json.contains("untrusted_fs_roots"), "{json}");
    }

    #[test]
    fn shell_allow_list_only_narrows() {
        let mut merged = MergedPolicy::builtin_defaults();
        // Overlay tries to add `npm` and keep `cargo`.
        let raw = RawPolicy::parse("[shell]\nallowed_programs = [\"cargo\", \"npm\"]").unwrap();
        merged.apply_untrusted_overlay(&raw);
        assert_eq!(merged.shell_allowed_programs, vec!["cargo".to_string()]);
    }

    /// FIX 2 (agent & tool fixes spec, §2a): the built-in default was Rust-only
    /// (`cargo`/`git`/`rg`/`rustfmt`), so basic exploration in a non-Rust repo
    /// (evidence: a Python repo review) denied `ls`/`find` outright. The default
    /// now also carries a curated, language-agnostic, read-only exploration set —
    /// while keeping the original four and excluding `find` (`-delete`/`-exec`
    /// can write/exec, bypassing the allow-list) and every interpreter/multiplexer.
    #[test]
    fn builtin_default_shell_allow_list_includes_curated_read_only_set() {
        let defaults = MergedPolicy::builtin_defaults();
        let expected = [
            // Original Rust baseline — unchanged.
            "cargo", "git", "rg", "rustfmt",
            // Curated read-only exploration set (FIX 2a).
            "ls", "cat", "head", "tail", "wc", "grep", "tree", "pwd", "file", "which", "sort",
            "uniq",
        ];
        for program in expected {
            assert!(
                defaults.shell_allowed_programs.iter().any(|p| p == program),
                "expected `{program}` in the default shell allow-list, got {:?}",
                defaults.shell_allowed_programs
            );
        }
        assert_eq!(
            defaults.shell_allowed_programs.len(),
            expected.len(),
            "no extra programs beyond the curated set: {:?}",
            defaults.shell_allowed_programs
        );
        // `find` is deliberately excluded: `-delete`/`-exec`/`-execdir`/`-ok` can
        // remove files or run arbitrary programs, bypassing the allow-list.
        assert!(!defaults.shell_allowed_programs.iter().any(|p| p == "find"));
    }

    #[test]
    fn git_and_network_ratchet_toward_restriction() {
        let mut merged = MergedPolicy::builtin_defaults();
        // Overlay tries to relax commit to `allow` — must not weaken approval.
        let raw =
            RawPolicy::parse("[git]\ncommit = \"allow\"\n[network]\ndefault = \"allow\"").unwrap();
        merged.apply_untrusted_overlay(&raw);
        assert_eq!(merged.git_commit, ApprovalAction::Approval);
        assert_eq!(merged.network_default, NetworkDefault::Deny);
    }

    /// PF1: a **trusted** global config may WIDEN the shell allow-list — the
    /// concrete `pytest` need. The merged set must contain both `pytest` and
    /// the built-in baseline (union, not replace).
    #[test]
    fn trusted_overlay_widens_shell_allow_list() {
        let mut merged = MergedPolicy::builtin_defaults();
        let raw = RawPolicy::parse("[shell]\nallowed_programs = [\"pytest\"]").unwrap();
        merged.apply_trusted_overlay(&raw);
        assert!(merged.shell_allowed_programs.iter().any(|p| p == "pytest"));
        // Built-ins survive — this is a union, not a replace.
        for builtin in ["cargo", "git", "rg", "rustfmt"] {
            assert!(
                merged.shell_allowed_programs.iter().any(|p| p == builtin),
                "expected `{builtin}` to survive the trusted widen, got {:?}",
                merged.shell_allowed_programs
            );
        }
    }

    /// PF1 regression guard: an **untrusted** repo-local file must still only
    /// narrow, even post-split. A repo cannot add `pytest` to the allow-list.
    #[test]
    fn untrusted_overlay_still_only_narrows_shell_allow_list() {
        let mut merged = MergedPolicy::builtin_defaults();
        let raw = RawPolicy::parse("[shell]\nallowed_programs = [\"cargo\", \"pytest\"]").unwrap();
        merged.apply_untrusted_overlay(&raw);
        assert_eq!(merged.shell_allowed_programs, vec!["cargo".to_string()]);
        assert!(!merged.shell_allowed_programs.iter().any(|p| p == "pytest"));
    }

    /// PF1: a trusted global config may WIDEN `fs_read` (e.g. to read a
    /// vendored directory under the user's home).
    ///
    /// D3 note: this now uses an anchored root (`$HOME/vendored`) rather than
    /// a bare absolute path (`/opt/vendored`, as PF1 originally wrote it).
    /// `normalize_raw` (routed through by `union_roots`) requires a
    /// recognized leading anchor (`$REPOSITORY`/`$WORKTREE`/`$HOME`) — a
    /// root with none is outside the model this merge-time check can reason
    /// about at all and is dropped, fail-closed, same as an escaping root.
    /// See `normalize_raw_drops_root_with_no_anchor`.
    #[test]
    fn trusted_overlay_widens_fs_read() {
        let mut merged = MergedPolicy::builtin_defaults();
        assert_eq!(merged.fs_read, vec!["$REPOSITORY".to_string()]);
        let raw = RawPolicy::parse("[filesystem]\nread = [\"$HOME/vendored\"]").unwrap();
        merged.apply_trusted_overlay(&raw);
        assert!(merged.fs_read.iter().any(|r| r == "$REPOSITORY"));
        assert!(merged.fs_read.iter().any(|r| r == "$HOME/vendored"));
    }

    /// PF1 SECURITY FLOOR (Decision 2a): `fs_write` never widens, even from
    /// the trusted global config. A trusted overlay naming `$HOME` must not
    /// add it to the write scope — worktree confinement is the one invariant
    /// no file, trusted or not, may relax.
    #[test]
    fn trusted_overlay_does_not_widen_fs_write() {
        let mut merged = MergedPolicy::builtin_defaults();
        assert_eq!(merged.fs_write, vec!["$WORKTREE".to_string()]);
        let raw = RawPolicy::parse("[filesystem]\nwrite = [\"$HOME\"]").unwrap();
        merged.apply_trusted_overlay(&raw);
        // `$HOME` is disjoint from `$WORKTREE`, so the narrow-only intersect
        // drops it entirely rather than widening the write scope.
        assert!(!merged.fs_write.iter().any(|r| r == "$HOME"));
        assert!(merged.fs_write.is_empty() || merged.fs_write == vec!["$WORKTREE".to_string()]);
    }

    /// PF1: a trusted global config may WIDEN the network allow-list and
    /// relax `network.default` and `git.commit` — the opposite of the
    /// untrusted ratchet proven by `git_and_network_ratchet_toward_restriction`.
    #[test]
    fn trusted_overlay_widens_network_and_relaxes_git() {
        let mut merged = MergedPolicy::builtin_defaults();
        let raw = RawPolicy::parse(
            "[network]\nallow = [\"example.com\"]\ndefault = \"allow\"\n[git]\ncommit = \"allow\"",
        )
        .unwrap();
        merged.apply_trusted_overlay(&raw);
        assert!(merged.network_allow.iter().any(|n| n == "example.com"));
        assert_eq!(merged.network_default, NetworkDefault::Allow);
        assert_eq!(merged.git_commit, ApprovalAction::Allow);
    }

    #[test]
    fn spec_file_parses_with_every_section() {
        let spec = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/specs/policy.toml");
        let contents = std::fs::read_to_string(spec).expect("read spec policy");
        let raw = RawPolicy::parse(&contents).expect("spec policy must parse");
        assert!(raw.filesystem.is_some());
        assert!(raw.shell.is_some());
        assert!(raw.git.is_some());
        assert!(raw.mcp.is_some());
        assert!(raw.plugins.is_some());
        assert!(raw.memory.is_some());
    }

    // --- PR B: the `[mcp]` section merge semantics ---

    /// The trusted global may set the default and per-server dispositions in
    /// either direction (relax or tighten), exactly like `[git]`.
    #[test]
    fn trusted_overlay_sets_mcp_dispositions_either_direction() {
        let mut merged = MergedPolicy::builtin_defaults();
        let raw = RawPolicy::parse(
            "[mcp]\ndefault = \"always-approval\"\n[mcp.servers]\ngithub = \"allow\"",
        )
        .unwrap();
        merged.apply_trusted_overlay(&raw);
        assert_eq!(merged.mcp_default, ApprovalAction::AlwaysApproval);
        assert_eq!(
            merged.mcp_servers.get("github"),
            Some(&ApprovalAction::Allow)
        );
    }

    /// An untrusted repo layer may tighten a server's disposition (or deny it
    /// outright) but never relax what the broader layers set — including
    /// "relaxing" a server that only inherited a strict default.
    #[test]
    fn untrusted_overlay_only_narrows_mcp_dispositions() {
        let mut merged = MergedPolicy::builtin_defaults();
        let trusted =
            RawPolicy::parse("[mcp]\ndefault = \"approval\"\n[mcp.servers]\ngithub = \"allow\"")
                .unwrap();
        merged.apply_trusted_overlay(&trusted);
        // The repo tries to relax the default to `allow` and tighten `github`
        // to `always-approval`. Only the tightening may take effect.
        let repo =
            RawPolicy::parse("[mcp]\ndefault = \"allow\"\n[mcp.servers]\ngithub = \"always-approval\"\nexperimental = \"deny\"")
                .unwrap();
        merged.apply_untrusted_overlay(&repo);
        assert_eq!(merged.mcp_default, ApprovalAction::Approval);
        assert_eq!(
            merged.mcp_servers.get("github"),
            Some(&ApprovalAction::AlwaysApproval)
        );
        assert_eq!(
            merged.mcp_servers.get("experimental"),
            Some(&ApprovalAction::Deny)
        );
    }

    /// A repo layer naming a server no broader layer mentioned narrows from the
    /// *default*: with the builtin `approval` default, `server = "allow"` must
    /// NOT take effect, but `server = "deny"` must.
    #[test]
    fn untrusted_overlay_new_server_narrows_from_default() {
        let mut merged = MergedPolicy::builtin_defaults();
        let repo =
            RawPolicy::parse("[mcp.servers]\nsneaky = \"allow\"\ncautious = \"deny\"").unwrap();
        merged.apply_untrusted_overlay(&repo);
        assert_eq!(
            merged.mcp_servers.get("sneaky"),
            Some(&ApprovalAction::Approval),
            "a repo layer must not relax below the default"
        );
        assert_eq!(
            merged.mcp_servers.get("cautious"),
            Some(&ApprovalAction::Deny)
        );
    }

    #[test]
    fn unknown_key_is_a_parse_error() {
        assert!(RawPolicy::parse("bogus_top_level = 1").is_err());
        assert!(RawPolicy::parse("[filesystem]\nbogus = 1").is_err());
    }

    // --- D3: `normalize_raw` (merge-time lexical collapse + anchor escape guard) ---

    /// D3 core case: `..` popping past the anchor is an escape — dropped.
    #[test]
    fn normalize_raw_drops_escape_above_anchor() {
        assert_eq!(normalize_raw("$WORKTREE/../etc"), None);
    }

    /// A `..` that only cancels an earlier, non-anchor component collapses
    /// safely and keeps the anchor.
    #[test]
    fn normalize_raw_collapses_safe_dotdot() {
        assert_eq!(
            normalize_raw("$WORKTREE/sub/../other"),
            Some("$WORKTREE/other".to_string())
        );
    }

    /// A bare anchor with no subpath normalizes to itself.
    #[test]
    fn normalize_raw_bare_anchor_is_unchanged() {
        assert_eq!(normalize_raw("$WORKTREE"), Some("$WORKTREE".to_string()));
    }

    /// Every recognized anchor is supported, and `.` components are dropped.
    #[test]
    fn normalize_raw_supports_every_anchor_and_drops_curdir() {
        assert_eq!(
            normalize_raw("$REPOSITORY/./sub"),
            Some("$REPOSITORY/sub".to_string())
        );
        assert_eq!(normalize_raw("$HOME/./"), Some("$HOME".to_string()));
    }

    /// A root with no recognized leading anchor is unsafe (out of the model
    /// this merge-time check can reason about) and is dropped, fail-closed.
    #[test]
    fn normalize_raw_drops_root_with_no_anchor() {
        assert_eq!(normalize_raw("/etc"), None);
        assert_eq!(normalize_raw("relative/path"), None);
    }

    /// A `..` chain that pops multiple levels above the anchor still escapes.
    #[test]
    fn normalize_raw_drops_multi_level_escape() {
        assert_eq!(normalize_raw("$HOME/../../etc"), None);
    }

    /// D3 regression: `intersect_roots` (the untrusted narrow-only path) must
    /// drop an escaping overlay root rather than admitting it via the old
    /// un-collapsed `raw_within` check.
    #[test]
    fn intersect_roots_drops_escaping_root() {
        let base = vec!["$WORKTREE".to_string()];
        let overlay = vec!["$WORKTREE/../etc".to_string()];
        let merged = intersect_roots(&base, &overlay);
        assert!(
            merged.is_empty(),
            "escaping root must be dropped, got {merged:?}"
        );
    }

    /// D3: a normal (non-escaping) overlay root still survives the
    /// intersection after normalization.
    #[test]
    fn intersect_roots_keeps_normal_root_after_normalization() {
        let base = vec!["$WORKTREE".to_string()];
        let overlay = vec!["$WORKTREE/src".to_string()];
        let merged = intersect_roots(&base, &overlay);
        assert_eq!(merged, vec!["$WORKTREE/src".to_string()]);
    }

    /// D3 regression: `union_roots` (the trusted widen path) must also drop
    /// an escaping overlay root — a trusted source widening with an
    /// escaping `..` must never be admitted either.
    #[test]
    fn union_roots_drops_escaping_root() {
        let base = vec!["$WORKTREE".to_string()];
        let overlay = vec!["$WORKTREE/../etc".to_string()];
        let merged = union_roots(&base, &overlay);
        assert!(
            !merged.iter().any(|r| r.contains("etc")),
            "escaping root must be dropped, got {merged:?}"
        );
        assert_eq!(merged, vec!["$WORKTREE".to_string()]);
    }

    /// D3: a normal (non-escaping) overlay root still survives the union.
    #[test]
    fn union_roots_keeps_normal_root_after_normalization() {
        let base = vec!["$REPOSITORY".to_string()];
        let overlay = vec!["$HOME/vendored".to_string()];
        let merged = union_roots(&base, &overlay);
        assert!(merged.iter().any(|r| r == "$REPOSITORY"));
        assert!(merged.iter().any(|r| r == "$HOME/vendored"));
    }

    /// D3 property: every root that survives `intersect_roots` is anchored
    /// and carries no residual `..` (i.e. re-normalizing it is a no-op) —
    /// merge-time containment never emits an unsafe string.
    #[test]
    fn intersect_roots_result_is_always_normalized() {
        let alphabet = [
            "$WORKTREE",
            "$WORKTREE/../etc",
            "$WORKTREE/src/../lib",
            "$REPOSITORY",
            "/tmp/evil",
            "$HOME/../../etc",
        ];
        for &b0 in &alphabet {
            for &o0 in &alphabet {
                let base = vec![b0.to_string()];
                let overlay = vec![o0.to_string()];
                let merged = intersect_roots(&base, &overlay);
                for root in &merged {
                    assert_eq!(
                        normalize_raw(root).as_deref(),
                        Some(root.as_str()),
                        "merged root {root} was not already normalized (base {base:?}, overlay {overlay:?})"
                    );
                }
            }
        }
    }
}
