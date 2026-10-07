//! `workspace.read_file` — a line-numbered excerpt of a file, confined to the
//! granted read scope.

use std::path::PathBuf;

use codypendent_daemon::policy::PathScope;
use codypendent_protocol::ProposedAction;

use super::{secure_fs, CapabilityKind, ToolError};

/// Default line ceiling when no explicit range is requested.
const DEFAULT_MAX_LINES: usize = 200;
/// Most lines one call returns, however wide a range was asked for. A model
/// that sends `range: [1, 9223372036854775807]` for a large file used to make
/// the daemon retain every line (tens of millions of `String`s, over 1.5 GB) and
/// push all of it into the transcript. The excerpt reports `end_line` and
/// `total_lines`, so the caller sees where the cut fell and asks for the next
/// window.
const MAX_RANGE_LINES: usize = 2_000;
/// Most characters of any one line returned. A minified bundle is a single
/// multi-megabyte line; the rest is replaced by a count of what was left out.
const MAX_LINE_CHARS: usize = 2_000;
/// Most bytes of numbered excerpt returned in one call, whatever the line count.
const MAX_EXCERPT_BYTES: usize = 128 * 1024;
/// Upper bound on the bytes the reader will produce, so a single pathological
/// line (e.g. a minified multi-hundred-MB file) can never be buffered whole.
/// Far larger than any real source file; content beyond it is not read.
const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

/// Typed input for [`ReadFile::execute`].
#[derive(Debug, Clone)]
pub struct ReadFileInput {
    /// The file to read.
    pub path: PathBuf,
    /// An optional inclusive 1-based `(start, end)` line range. When absent, the
    /// first [`DEFAULT_MAX_LINES`] lines are returned.
    pub range: Option<(usize, usize)>,
}

/// A line-numbered excerpt of a file.
#[derive(Debug, Clone)]
pub struct FileExcerpt {
    /// The file the excerpt came from.
    pub path: PathBuf,
    /// First line included (1-based).
    pub start_line: usize,
    /// Last line included (1-based, inclusive).
    pub end_line: usize,
    /// Total lines in the file.
    pub total_lines: usize,
    /// Whether the file has content beyond the returned excerpt.
    pub truncated: bool,
    /// The excerpt, each line prefixed with its 1-based number.
    pub content: String,
}

/// The `workspace.read_file` tool.
pub struct ReadFile;

impl ReadFile {
    /// The stable tool name.
    pub const NAME: &'static str = "workspace.read_file";

    /// Capability classes this tool draws on.
    pub fn required_capabilities() -> &'static [CapabilityKind] {
        &[CapabilityKind::FileRead]
    }

    /// The [`ProposedAction`] the middleware evaluates before granting.
    pub fn proposed_action(input: &ReadFileInput) -> ProposedAction {
        ProposedAction::ReadFiles {
            paths: vec![input.path.to_string_lossy().into_owned()],
        }
    }

    /// Read an excerpt of `input.path`, refusing any path outside `scope`.
    ///
    /// The path is canonicalized *once*, the scope check runs on that resolved
    /// path, and the very same resolved path is then opened and streamed — so a
    /// traversal or a symlink swapped in between the check and the open cannot
    /// redirect the read out of scope (no TOCTOU gap). The file is read line by
    /// line through a [`tokio::io::BufReader`], retaining only the excerpt window
    /// in memory, so an enormous file is never buffered whole. At most
    /// [`DEFAULT_MAX_LINES`] lines are returned unless an explicit range is given.
    pub async fn execute(
        input: &ReadFileInput,
        scope: &PathScope,
    ) -> Result<FileExcerpt, ToolError> {
        use tokio::io::AsyncBufReadExt;

        // Open descriptor-relative to the authorized root, refusing symlinks at
        // every component. Subsequent reads use this handle, never the pathname.
        let path = input.path.clone();
        let scope_for_open = scope.clone();
        let scoped_res =
            tokio::task::spawn_blocking(move || secure_fs::open_read(&path, &scope_for_open))
                .await
                .map_err(|error| {
                    ToolError::Other(anyhow::anyhow!("read worker failed: {error}"))
                })?;

        let scoped = match scoped_res {
            Ok(s) => s,
            Err(ToolError::Io(ref err)) if err.kind() == std::io::ErrorKind::NotFound => {
                let p = input.path.clone();
                let s = scope.clone();
                let suggestions = tokio::task::spawn_blocking(move || did_you_mean(&p, &s))
                    .await
                    .unwrap_or_default();
                return Err(ToolError::FileNotFound {
                    path: input.path.clone(),
                    suggestions,
                });
            }
            Err(other) => return Err(other),
        };

        // Validate an explicit range before touching the file (unchanged errors).
        if let Some((start, end)) = input.range {
            if start == 0 {
                return Err(ToolError::InvalidRange {
                    start,
                    end,
                    reason: "line numbers are 1-based".to_string(),
                });
            }
            if end < start {
                return Err(ToolError::InvalidRange {
                    start,
                    end,
                    reason: "end precedes start".to_string(),
                });
            }
        }

        // The inclusive window we retain: the requested span, or the first
        // DEFAULT_MAX_LINES lines by default. Only these lines are held in memory.
        let (want_start, requested_end) = match input.range {
            Some((start, end)) => (start, end),
            None => (1, DEFAULT_MAX_LINES),
        };
        // Cap the window itself, before anything is retained: `end - start` is
        // model-controlled and was used as-is.
        let want_end = requested_end.min(want_start.saturating_add(MAX_RANGE_LINES - 1));

        // Refuse non-regular files: a FIFO/device inside the scope would block
        // the read forever (a pipe never reaches EOF while a writer can appear).
        let metadata = scoped.file.metadata()?;
        if !metadata.is_file() {
            return Err(ToolError::Other(anyhow::anyhow!(
                "not a regular file: {}",
                scoped.path.display()
            )));
        }

        // Stream line by line, keeping only the window and counting the total, so
        // the excerpt semantics (total_lines, truncation) stay exact without the
        // whole file ever residing in memory. The reader is byte-bounded so one
        // enormous newline-free line cannot be buffered whole either.
        let file = tokio::fs::File::from_std(scoped.file);
        let bounded = tokio::io::AsyncReadExt::take(file, MAX_READ_BYTES);
        let mut lines = tokio::io::BufReader::new(bounded).lines();
        let mut total = 0usize;
        let mut window: Vec<String> = Vec::new();
        while let Some(line) = lines.next_line().await? {
            total += 1;
            if total >= want_start && total <= want_end {
                window.push(line);
            }
            // Past the window we only keep counting (nothing is retained).
        }

        let (start, end) = if total == 0 || want_start > total {
            (0, 0)
        } else {
            (want_start, want_end.min(total))
        };

        // Emit the retained lines whose absolute number falls in [start, end].
        // The window's first entry is line `want_start`. Output stops at the
        // byte budget, and `end` is pulled back to the last line actually shown
        // so the excerpt never claims lines it did not include.
        let mut content = String::new();
        let mut end = end;
        if start > 0 {
            let mut last_shown = start - 1;
            for (offset, line) in window.iter().enumerate() {
                let number = want_start + offset;
                if number < start {
                    continue;
                }
                if number > end {
                    break;
                }
                let rendered = format!("{number:>6}\t{}\n", clip_line(line));
                // Always show at least one line, however wide.
                if last_shown >= start && content.len() + rendered.len() > MAX_EXCERPT_BYTES {
                    break;
                }
                content.push_str(&rendered);
                last_shown = number;
            }
            end = last_shown;
        }

        Ok(FileExcerpt {
            path: input.path.clone(),
            start_line: start,
            end_line: end,
            total_lines: total,
            truncated: end < total || start > 1,
            content,
        })
    }
}

/// `line`, or its first [`MAX_LINE_CHARS`] characters followed by how many were
/// dropped. Cuts on a character boundary.
fn clip_line(line: &str) -> std::borrow::Cow<'_, str> {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => {
            let dropped = line[cut..].chars().count();
            std::borrow::Cow::Owned(format!(
                "{}… [{dropped} more characters not shown]",
                &line[..cut]
            ))
        }
        None => std::borrow::Cow::Borrowed(line),
    }
}

/// Up to 3 entries of `path`'s parent whose lowercase name contains the
/// requested leaf's lowercase name or vice versa. The parent is re-checked
/// against `scope` and read with std::fs::read_dir on the RESOLVED parent;
/// any error yields no suggestions (the not-found error stands alone).
fn did_you_mean(path: &std::path::Path, scope: &PathScope) -> Vec<String> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let parent_to_check = if parent.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        parent
    };
    let (resolved_parent, verdict) = scope.resolve(parent_to_check);
    if verdict != codypendent_daemon::policy::ScopeVerdict::Allowed {
        return Vec::new();
    }
    let Some(leaf) = path.file_name().and_then(|f| f.to_str()) else {
        return Vec::new();
    };
    let leaf_lower = leaf.to_lowercase();
    let Ok(entries) = std::fs::read_dir(&resolved_parent) else {
        return Vec::new();
    };

    let mut suggestions = Vec::new();
    for entry in entries.flatten() {
        if let Ok(file_name) = entry.file_name().into_string() {
            let name_lower = file_name.to_lowercase();
            if name_lower != leaf_lower && is_similar(&name_lower, &leaf_lower) {
                suggestions.push(file_name);
            }
        }
    }
    suggestions.sort();
    suggestions.truncate(3);
    suggestions
}

fn is_similar(a: &str, b: &str) -> bool {
    if a.contains(b) || b.contains(a) {
        return true;
    }
    levenshtein_distance(a, b) <= 2
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();
    let mut dp = vec![vec![0; n + 1]; m + 1];

    for (i, row) in dp.iter_mut().enumerate().take(m + 1) {
        row[0] = i;
    }
    if let Some(first_row) = dp.first_mut() {
        for (j, cell) in first_row.iter_mut().enumerate().take(n + 1) {
            *cell = j;
        }
    }

    for i in 1..=m {
        for j in 1..=n {
            if a_chars[i - 1] == b_chars[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            } else {
                dp[i][j] = 1 + dp[i - 1][j].min(dp[i][j - 1]).min(dp[i - 1][j - 1]);
            }
        }
    }
    dp[m][n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_daemon::policy::PathScope;

    #[tokio::test]
    async fn not_found_suggests_similar_sibling_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();

        let scope = PathScope::new(vec![root.clone()], vec![]);
        let err = ReadFile::execute(
            &ReadFileInput {
                path: root.join("src/mian.rs"),
                range: None,
            },
            &scope,
        )
        .await
        .unwrap_err();

        match err {
            ToolError::FileNotFound {
                ref path,
                ref suggestions,
            } => {
                assert_eq!(path, &root.join("src/mian.rs"));
                assert_eq!(suggestions, &vec!["main.rs".to_string()]);
                let msg = err.to_string();
                assert!(msg.contains("file not found:"));
                assert!(msg.contains("— did you mean: main.rs?"));
            }
            other => panic!("expected FileNotFound, got {other:?}"),
        }
    }

    async fn read(path: PathBuf, range: Option<(usize, usize)>, scope: &PathScope) -> FileExcerpt {
        ReadFile::execute(&ReadFileInput { path, range }, scope)
            .await
            .expect("read")
    }

    #[tokio::test]
    async fn an_unbounded_range_is_capped_and_reports_where_the_cut_fell() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let body: String = (1..=5_000).map(|n| format!("line {n}\n")).collect();
        std::fs::write(root.join("big.txt"), body).unwrap();
        let scope = PathScope::new(vec![root.clone()], vec![]);

        // The shape a model can send: an end far past anything real.
        let excerpt = read(root.join("big.txt"), Some((1, usize::MAX)), &scope).await;
        assert_eq!(excerpt.start_line, 1);
        assert_eq!(excerpt.end_line, MAX_RANGE_LINES);
        assert_eq!(excerpt.total_lines, 5_000);
        assert!(excerpt.truncated, "the cut must be visible to the caller");
        assert_eq!(excerpt.content.lines().count(), MAX_RANGE_LINES);

        // The next window picks up where the first stopped.
        let next = read(
            root.join("big.txt"),
            Some((excerpt.end_line + 1, usize::MAX)),
            &scope,
        )
        .await;
        assert_eq!(next.start_line, MAX_RANGE_LINES + 1);
        assert_eq!(next.end_line, 2 * MAX_RANGE_LINES);
    }

    #[tokio::test]
    async fn a_window_ending_inside_the_cap_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let body: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        std::fs::write(root.join("small.txt"), body).unwrap();
        let scope = PathScope::new(vec![root.clone()], vec![]);

        let excerpt = read(root.join("small.txt"), Some((10, 20)), &scope).await;
        assert_eq!((excerpt.start_line, excerpt.end_line), (10, 20));
        assert_eq!(excerpt.total_lines, 50);
        assert!(excerpt.content.starts_with("    10\tline 10\n"));
    }

    #[tokio::test]
    async fn one_enormous_line_is_clipped_not_returned_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("bundle.min.js"),
            format!("{}\nsecond\n", "x".repeat(500_000)),
        )
        .unwrap();
        let scope = PathScope::new(vec![root.clone()], vec![]);

        let excerpt = read(root.join("bundle.min.js"), Some((1, 2)), &scope).await;
        assert!(
            excerpt.content.len() < 4_096,
            "got {} bytes",
            excerpt.content.len()
        );
        assert!(excerpt.content.contains("more characters not shown"));
        assert!(excerpt.content.contains("second"));
    }

    #[tokio::test]
    async fn the_excerpt_stops_at_the_byte_budget_and_end_line_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        // 1,500 lines of ~1,000 characters: under the line cap, over the byte cap.
        let body: String = (1..=1_500)
            .map(|n| format!("{n:04} {}\n", "y".repeat(1_000)))
            .collect();
        std::fs::write(root.join("wide.txt"), body).unwrap();
        let scope = PathScope::new(vec![root.clone()], vec![]);

        let excerpt = read(root.join("wide.txt"), Some((1, 1_500)), &scope).await;
        assert!(excerpt.content.len() <= MAX_EXCERPT_BYTES);
        assert!(excerpt.end_line < 1_500, "ended at {}", excerpt.end_line);
        assert_eq!(excerpt.content.lines().count(), excerpt.end_line);
        assert!(excerpt.truncated);
    }

    #[tokio::test]
    async fn not_found_out_of_scope_yields_no_suggestions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_root = outside.path().canonicalize().unwrap();
        std::fs::write(outside_root.join("main.rs"), "fn main() {}").unwrap();

        let scope = PathScope::new(vec![root], vec![]);
        let err = ReadFile::execute(
            &ReadFileInput {
                path: outside_root.join("mian.rs"),
                range: None,
            },
            &scope,
        )
        .await
        .unwrap_err();

        // Either path out of scope or file not found without suggestions
        match err {
            ToolError::PathOutOfScope(_) => {}
            ToolError::FileNotFound { suggestions, .. } => {
                assert!(suggestions.is_empty());
            }
            other => panic!("unexpected error {other:?}"),
        }
    }
}
