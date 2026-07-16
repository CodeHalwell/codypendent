//! `workspace.read_file` — a line-numbered excerpt of a file, confined to the
//! granted read scope.

use std::path::{Path, PathBuf};

use codypendent_daemon::policy::{PathScope, ScopeVerdict};
use codypendent_protocol::ProposedAction;

use super::{CapabilityKind, ToolError};

/// Default line ceiling when no explicit range is requested.
const DEFAULT_MAX_LINES: usize = 200;

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
    /// The path is canonicalized and classified against the granted scope before
    /// the file is opened — a traversal or planted symlink cannot smuggle a read
    /// out of scope. At most [`DEFAULT_MAX_LINES`] lines are returned unless an
    /// explicit range is given.
    pub async fn execute(
        input: &ReadFileInput,
        scope: &PathScope,
    ) -> Result<FileExcerpt, ToolError> {
        Self::guard_scope(&input.path, scope)?;

        let bytes = tokio::fs::read(&input.path).await?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        let (start, end) = match input.range {
            Some((start, end)) => {
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
                (start, end.min(total.max(1)))
            }
            None => (1, total.clamp(1, DEFAULT_MAX_LINES)),
        };

        // Clamp to the file; an empty file yields an empty excerpt.
        let (start, end) = if total == 0 {
            (0, 0)
        } else {
            (start.min(total), end.min(total))
        };

        let mut content = String::new();
        if total > 0 {
            for (offset, line) in lines[start - 1..end].iter().enumerate() {
                let number = start + offset;
                content.push_str(&format!("{number:>6}\t{line}\n"));
            }
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

    /// Canonicalize and classify `path`, mapping the verdict to a refusal.
    fn guard_scope(path: &Path, scope: &PathScope) -> Result<(), ToolError> {
        match scope.classify(path) {
            ScopeVerdict::Allowed => Ok(()),
            ScopeVerdict::Denied => Err(ToolError::PathDenied(path.to_path_buf())),
            ScopeVerdict::OutsideRoots => Err(ToolError::PathOutOfScope(path.to_path_buf())),
        }
    }
}
