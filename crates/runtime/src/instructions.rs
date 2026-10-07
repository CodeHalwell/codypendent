//! Hierarchical instruction-file discovery (AGENTS.md / CLAUDE.md / .codypendent),
//! ported from codex `agents_md.rs`: walk cwd → project root, concatenate root
//! first so the most specific (cwd) file wins. Never walk past the project root.
//!
//! # Trust
//!
//! The text found here is prepended to the model's system prompt, sent to the
//! provider and journaled, so *which bytes get read* is a security decision:
//!
//! * A file inside the repository is attacker-controlled (a cloned repo can
//!   commit anything). It is read only if its **canonical** path stays inside
//!   the canonical project root and it is a regular file — a committed
//!   `CLAUDE.md -> ~/.aws/credentials` is refused rather than exfiltrated, and
//!   `-> /dev/zero` or a FIFO cannot exhaust memory or hang the run. A symlink
//!   that stays inside the repository (the common `CLAUDE.md -> AGENTS.md`) is
//!   followed, and the two names are included once.
//! * The user's own `~/.claude/CLAUDE.md` may legitimately be a symlink into a
//!   dotfiles checkout, so it is followed anywhere — but must still be a
//!   regular file and is read with the same bound.
//! * Every read is byte-bounded; no file is slurped whole.
//!
//! # Budget
//!
//! The concatenation is capped at [`MAX_INSTRUCTION_BYTES`]. When the files do
//! not all fit, the **most specific** ones are kept (a suffix of the precedence
//! order, so the result is monotonic) and the rest are dropped with an explicit
//! marker the model can see and a warning in the log. Dropping the cwd's
//! `AGENTS.md` in favour of a large global file would invert the precedence this
//! module exists to implement.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Files read at each directory, in fixed precedence order (later = more specific).
pub const INSTRUCTION_FILENAMES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
    ".cursorrules",
    ".clinerules",
    ".windsurfrules",
    ".github/copilot-instructions.md",
];

/// Markers that identify the project root (traversal stops at the first match).
pub const PROJECT_ROOT_MARKERS: &[&str] = &[".git", ".codypendent"];

/// Separator placed between instruction blocks.
const SEPARATOR: &str = "\n\n--- instructions ---\n\n";

/// Cap the concatenation so a stray large file cannot bloat every prompt.
pub const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;

/// Upper bound on the length of the omission marker, reserved out of the budget
/// whenever something has to be dropped so the cap holds for the final text.
const MARKER_RESERVE: usize = 192;

/// Where a candidate file lives, which decides how far a symlink may lead.
#[derive(Clone, Copy)]
enum Origin<'a> {
    /// The user's own configuration: symlinks are followed wherever they point.
    User,
    /// Repository content: the canonical path must stay under this canonical root.
    Repository(&'a Path),
}

/// What reading one candidate produced.
enum Read {
    Text(String),
    /// Larger than [`MAX_INSTRUCTION_BYTES`] on its own.
    TooLarge,
    /// Missing, not a regular file, outside the allowed root, or not UTF-8.
    Skipped,
}

/// One accepted instruction file, in precedence order.
struct Found {
    path: PathBuf,
    body: String,
}

/// Discover and concatenate instructions for a run rooted at `cwd`. Returns
/// `None` when nothing is found (so the caller leaves the system prompt as-is).
#[must_use]
pub fn discover_instructions(cwd: &Path, home: Option<&Path>) -> Option<String> {
    let root = project_root(cwd);
    // Without a canonical root no repository file can be proven to stay inside
    // it, so none is read; the user's own file is unaffected.
    let canonical_root = root.canonicalize().ok();

    let mut found: Vec<Found> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut omitted: Vec<PathBuf> = Vec::new();

    // Global layer first (lowest precedence), opencode-style.
    if let Some(home) = home {
        collect(
            &home.join(".claude/CLAUDE.md"),
            Origin::User,
            &mut seen,
            &mut found,
            &mut omitted,
        );
    }
    // Project layer: root → cwd inclusive, so cwd files land last.
    if let Some(canonical_root) = canonical_root.as_deref() {
        for dir in chain_root_to_cwd(&root, cwd) {
            for name in INSTRUCTION_FILENAMES {
                collect(
                    &dir.join(name),
                    Origin::Repository(canonical_root),
                    &mut seen,
                    &mut found,
                    &mut omitted,
                );
            }
            collect(
                &dir.join(".codypendent/instructions.md"),
                Origin::Repository(canonical_root),
                &mut seen,
                &mut found,
                &mut omitted,
            );
        }
    }

    fit_to_budget(found, omitted)
}

fn project_root(cwd: &Path) -> PathBuf {
    for dir in cwd.ancestors() {
        if PROJECT_ROOT_MARKERS.iter().any(|m| dir.join(m).exists()) {
            return dir.to_path_buf();
        }
    }
    cwd.to_path_buf() // no marker: only cwd is considered
}

/// Directories from project root down to cwd, inclusive, root first.
fn chain_root_to_cwd(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    chain.reverse(); // ancestors() is cwd→root; we want root→cwd
    chain
}

/// Read one candidate and record it. A file that resolves to a path already
/// taken (the `CLAUDE.md -> AGENTS.md` alias) is included once.
fn collect(
    path: &Path,
    origin: Origin<'_>,
    seen: &mut HashSet<PathBuf>,
    found: &mut Vec<Found>,
    omitted: &mut Vec<PathBuf>,
) {
    // `symlink_metadata` first: the overwhelmingly common case is "no such
    // file", and it must stay free of warnings.
    if std::fs::symlink_metadata(path).is_err() {
        return;
    }
    let Ok(canonical) = path.canonicalize() else {
        return; // dangling symlink
    };
    if let Origin::Repository(root) = origin {
        if !canonical.starts_with(root) {
            tracing::warn!(
                file = %path.display(),
                resolves_to = %canonical.display(),
                "ignoring an instruction file that resolves outside the repository"
            );
            return;
        }
    }
    if !seen.insert(canonical.clone()) {
        return;
    }
    match read_regular_file(&canonical) {
        Read::Text(body) => {
            let body = body.trim();
            if !body.is_empty() {
                found.push(Found {
                    path: path.to_path_buf(),
                    body: body.to_string(),
                });
            }
        }
        Read::TooLarge => {
            tracing::warn!(
                file = %path.display(),
                limit = MAX_INSTRUCTION_BYTES,
                "instruction file exceeds the instruction budget on its own; it was not loaded"
            );
            omitted.push(path.to_path_buf());
        }
        Read::Skipped => {}
    }
}

/// Open `path` (already canonical) without following a final symlink or blocking
/// on a FIFO, and read at most one byte more than [`MAX_INSTRUCTION_BYTES`].
fn read_regular_file(path: &Path) -> Read {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // NOFOLLOW: `path` was canonicalised moments ago, so a symlink here was
        // swapped in afterwards. NONBLOCK: opening a FIFO that has no writer
        // must not hang the run before it starts.
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    let Ok(file) = options.open(path) else {
        return Read::Skipped;
    };
    match file.metadata() {
        Ok(metadata) if metadata.is_file() => {}
        _ => return Read::Skipped,
    }
    // One byte past the cap distinguishes "exactly at the cap" from "over it"
    // without ever reading an unbounded file.
    let mut bytes = Vec::new();
    if file
        .take(MAX_INSTRUCTION_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Read::Skipped;
    }
    if bytes.len() > MAX_INSTRUCTION_BYTES {
        return Read::TooLarge;
    }
    match String::from_utf8(bytes) {
        Ok(text) => Read::Text(text),
        Err(_) => Read::Skipped,
    }
}

/// Join `found` (lowest precedence first) under [`MAX_INSTRUCTION_BYTES`],
/// keeping the most specific files when they do not all fit.
fn fit_to_budget(found: Vec<Found>, mut omitted: Vec<PathBuf>) -> Option<String> {
    let total: usize = found.iter().map(|f| f.body.len()).sum::<usize>()
        + SEPARATOR.len() * found.len().saturating_sub(1);

    let kept_from = if total <= MAX_INSTRUCTION_BYTES {
        0
    } else {
        // Keep the longest suffix (most specific files) that fits next to the
        // marker. A suffix, not a greedy fill: a small low-precedence file must
        // never be kept after a more specific one was dropped.
        let budget = MAX_INSTRUCTION_BYTES.saturating_sub(MARKER_RESERVE + SEPARATOR.len());
        let mut used = 0usize;
        let mut from = found.len();
        for (index, file) in found.iter().enumerate().rev() {
            let cost = file.body.len() + if used == 0 { 0 } else { SEPARATOR.len() };
            if used + cost > budget {
                break;
            }
            used += cost;
            from = index;
        }
        from
    };

    for dropped in &found[..kept_from] {
        tracing::warn!(
            file = %dropped.path.display(),
            limit = MAX_INSTRUCTION_BYTES,
            "dropping a lower-precedence instruction file to stay within the instruction budget"
        );
        omitted.push(dropped.path.clone());
    }

    let mut out = String::new();
    if !omitted.is_empty() {
        out.push_str(&format!(
            "[{} instruction file(s) omitted: the instruction budget is {} KiB, and the most \
             specific files are kept first]",
            omitted.len(),
            MAX_INSTRUCTION_BYTES / 1024
        ));
    }
    for file in &found[kept_from..] {
        if !out.is_empty() {
            out.push_str(SEPARATOR);
        }
        out.push_str(&file.body);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_root_to_cwd_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        std::fs::write(root.join("AGENTS.md"), "R").unwrap();
        std::fs::write(sub.join("CLAUDE.md"), "S").unwrap();

        let discovered = discover_instructions(&sub, None);
        assert_eq!(
            discovered,
            Some("R\n\n--- instructions ---\n\nS".to_string())
        );
    }

    #[test]
    fn does_not_cross_project_root() {
        let temp = tempfile::tempdir().unwrap();
        let above = temp.path().join("above");
        let root = above.join("root");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(above.join("AGENTS.md"), "ABOVE_ROOT").unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        std::fs::write(root.join("AGENTS.md"), "ROOT_RULES").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "SUB_RULES").unwrap();

        let discovered = discover_instructions(&sub, None);
        assert_eq!(
            discovered,
            Some("ROOT_RULES\n\n--- instructions ---\n\nSUB_RULES".to_string())
        );
        assert!(!discovered.as_ref().unwrap().contains("ABOVE_ROOT"));
    }

    #[test]
    fn no_files_returns_none() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();

        let discovered = discover_instructions(&root, None);
        assert_eq!(discovered, None);
    }

    #[test]
    fn a_file_over_the_budget_is_omitted_visibly_not_silently() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();

        // A single file exceeding MAX_INSTRUCTION_BYTES is not loaded, and the
        // model is told something was left out rather than seeing nothing.
        let huge = "x".repeat(MAX_INSTRUCTION_BYTES + 1);
        std::fs::write(root.join("AGENTS.md"), &huge).unwrap();
        let discovered = discover_instructions(&root, None).expect("a marker, not silence");
        assert!(discovered.contains("1 instruction file(s) omitted"));
        assert!(!discovered.contains("xxxx"));

        // Exactly at the cap is still loaded whole.
        let exact = "y".repeat(MAX_INSTRUCTION_BYTES);
        std::fs::write(root.join("AGENTS.md"), &exact).unwrap();
        assert_eq!(discover_instructions(&root, None), Some(exact));
    }

    #[test]
    fn the_most_specific_file_wins_the_budget() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();

        // 40K at the root + 30K in the cwd cannot both fit under 64K. The cwd
        // file is the more specific one, so IT is kept and the root's is dropped
        // — the old behaviour kept the root file and silently lost the cwd's.
        let first = "a".repeat(40 * 1024);
        let second = "b".repeat(30 * 1024);
        std::fs::write(root.join("AGENTS.md"), &first).unwrap();
        std::fs::write(sub.join("AGENTS.md"), &second).unwrap();

        let discovered = discover_instructions(&sub, None).unwrap();
        assert!(discovered.contains(&second));
        assert!(!discovered.contains("aaaa"));
        assert!(discovered.contains("1 instruction file(s) omitted"));
        assert!(discovered.len() <= MAX_INSTRUCTION_BYTES);
    }

    #[test]
    fn the_kept_files_are_a_suffix_of_the_precedence_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();

        // AGENTS.md (big) < CLAUDE.md (big) < .cursorrules (tiny). The two big
        // ones cannot share the budget; keeping the tiny, most specific file is
        // right, but dropping AGENTS.md while keeping a *lower* file than the
        // one dropped would not be. Here the tiny file is the highest precedence
        // so the kept set is {.cursorrules}, and CLAUDE.md goes with AGENTS.md.
        std::fs::write(root.join("AGENTS.md"), "a".repeat(40 * 1024)).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "c".repeat(40 * 1024)).unwrap();
        std::fs::write(root.join(".cursorrules"), "TINY").unwrap();

        let discovered = discover_instructions(&root, None).unwrap();
        assert!(discovered.contains("TINY"));
        assert!(!discovered.contains("2 instruction file(s) omitted"));
        // CLAUDE.md fits next to TINY (40K + 4B < 64K), AGENTS.md does not.
        assert!(discovered.contains(&"c".repeat(40 * 1024)));
        assert!(!discovered.contains("aaaa"));
        assert!(discovered.contains("1 instruction file(s) omitted"));
    }

    #[test]
    fn an_oversized_global_file_does_not_starve_the_project_file() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude/CLAUDE.md"),
            "g".repeat(MAX_INSTRUCTION_BYTES + 10),
        )
        .unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        std::fs::write(root.join("AGENTS.md"), "PROJECT_RULES").unwrap();

        let discovered = discover_instructions(&root, Some(&home)).unwrap();
        assert!(discovered.contains("PROJECT_RULES"));
        assert!(discovered.contains("1 instruction file(s) omitted"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_leading_outside_the_repository_is_never_read() {
        let temp = tempfile::tempdir().unwrap();
        let secret = temp.path().join("credentials");
        std::fs::write(&secret, "aws_secret_access_key=SHOULD_NEVER_REACH_A_PROMPT").unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("CLAUDE.md")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "HONEST_RULES").unwrap();

        let discovered = discover_instructions(&root, None).unwrap();
        assert_eq!(discovered, "HONEST_RULES");
        assert!(!discovered.contains("SHOULD_NEVER_REACH_A_PROMPT"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_device_is_not_read() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        // Outside the repository, so refused on containment; the regular-file
        // check is what would still save a user-owned symlink to a device.
        std::os::unix::fs::symlink("/dev/zero", root.join("AGENTS.md")).unwrap();
        assert_eq!(discover_instructions(&root, None), None);

        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", home.join(".claude/CLAUDE.md")).unwrap();
        assert_eq!(discover_instructions(&root, Some(&home)), None);
    }

    #[cfg(unix)]
    #[test]
    fn an_in_repository_alias_is_followed_and_included_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        std::fs::write(root.join("AGENTS.md"), "SHARED_RULES").unwrap();
        // The common `ln -s AGENTS.md CLAUDE.md` setup.
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();

        let discovered = discover_instructions(&root, None).unwrap();
        assert_eq!(discovered, "SHARED_RULES");
    }

    #[cfg(unix)]
    #[test]
    fn the_users_own_global_file_may_be_a_symlink_anywhere() {
        let temp = tempfile::tempdir().unwrap();
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("claude.md"), "GLOBAL_FROM_DOTFILES").unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("claude.md"), home.join(".claude/CLAUDE.md"))
            .unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();

        assert_eq!(
            discover_instructions(&root, Some(&home)),
            Some("GLOBAL_FROM_DOTFILES".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_cannot_hang_discovery() {
        use std::sync::mpsc;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        let fifo = root.join("AGENTS.md");
        // `mkfifo(1)` rather than a raw syscall: rustix has no `mknodat` on macOS.
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !matches!(made, Ok(status) if status.success()) {
            return; // no mkfifo on this host; nothing to prove
        }

        let (tx, rx) = mpsc::channel();
        let worker_root = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(discover_instructions(&worker_root, None));
        });
        let discovered = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("opening a FIFO with no writer must not block discovery");
        assert_eq!(discovered, None);
    }

    #[test]
    fn global_claude_md_is_lowest_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let claude_dir = home.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("CLAUDE.md"), "GLOBAL_INSTRUCTIONS").unwrap();

        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        std::fs::write(root.join("AGENTS.md"), "PROJECT_INSTRUCTIONS").unwrap();

        let discovered = discover_instructions(&root, Some(&home));
        assert_eq!(
            discovered,
            Some("GLOBAL_INSTRUCTIONS\n\n--- instructions ---\n\nPROJECT_INSTRUCTIONS".to_string())
        );
    }

    #[test]
    fn dot_codypendent_instructions_and_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let dot_cody = root.join(".codypendent");
        std::fs::create_dir_all(&dot_cody).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();
        std::fs::write(root.join("AGENTS.md"), "1_AGENTS").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "2_CLAUDE").unwrap();
        std::fs::write(dot_cody.join("instructions.md"), "3_CODYPENDENT").unwrap();

        let discovered = discover_instructions(&root, None);
        assert_eq!(
            discovered,
            Some("1_AGENTS\n\n--- instructions ---\n\n2_CLAUDE\n\n--- instructions ---\n\n3_CODYPENDENT".to_string())
        );
    }

    #[test]
    fn no_marker_only_checks_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(parent.join("AGENTS.md"), "PARENT_RULES").unwrap();
        std::fs::write(child.join("AGENTS.md"), "CHILD_RULES").unwrap();

        let discovered = discover_instructions(&child, None);
        assert_eq!(discovered, Some("CHILD_RULES".to_string()));
    }
}
