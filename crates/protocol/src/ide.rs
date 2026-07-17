//! IDE-awareness wire types (Phase 3 STEP 3.4).
//!
//! The daemon owns the session; IDE extensions are thin, editor-aware clients
//! ([Chapter 10](../../../docs/docs/10-ide-github-and-inputs.md)). An IDE pushes
//! its live context — active file, selection, open documents, and *digests* of
//! unsaved ("dirty") buffers — as an [`IdeContextUpdate`]; the daemon may push
//! back [`IdeRequest`]s (apply an edit, reveal a location, show a diff).
//!
//! [`SourceProvenance`] is the normative label every file excerpt entering model
//! context carries, so a client can always answer "where did this text come
//! from?" — a committed revision, the working tree, an unsaved editor buffer, a
//! generated patch, or an agent's worktree.
//!
//! Every enum here follows the crate idiom: internally tagged
//! (`#[serde(tag = "type")]`), `#[non_exhaustive]`, with a trailing
//! `#[serde(other)] Unknown` so a value from a newer peer degrades gracefully.

use serde::{Deserialize, Serialize};

/// A zero-based position in a text document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A half-open range within a single document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// The editor's current selection: a range within one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorSelection {
    pub path: String,
    pub range: Range,
}

/// A content digest for one unsaved ("dirty") editor buffer. The filesystem is
/// not always the user's current truth; the IDE sends digests so the daemon can
/// detect divergence and request the full contents only when required and
/// authorized (Chapter 10, "Unsaved buffers").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyBufferDigest {
    pub path: String,
    /// Lowercase hex SHA-256 of the buffer's current bytes.
    pub sha256: String,
    pub byte_length: u64,
}

/// A debounced snapshot of the IDE's context, pushed client→daemon. Clients
/// debounce these (≥ 300 ms) so a burst of keystrokes collapses to one update.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IdeContextUpdate {
    /// The file the user is focused on, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_file: Option<String>,
    /// The current selection, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<EditorSelection>,
    /// Paths of all open documents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_files: Vec<String>,
    /// Digests of every unsaved buffer (contents are never sent unsolicited).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirty_buffers: Vec<DirtyBufferDigest>,
    /// A monotonically increasing revision for the diagnostics set, so the
    /// daemon can tell whether it holds the latest without transferring them.
    #[serde(default)]
    pub diagnostics_revision: u64,
}

/// A point in a workspace the daemon asks the IDE to reveal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

/// A single text replacement within one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub path: String,
    pub range: Range,
    pub new_text: String,
}

/// A set of edits the daemon asks the IDE to apply. The IDE applies them
/// semantically in the editor; it never executes tools itself (invariant 2).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceEdit {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<TextEdit>,
}

/// A request to display a diff between two named sides in the IDE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRequest {
    pub title: String,
    /// A short label for each side (e.g. `HEAD` vs `proposed`).
    pub left_label: String,
    pub right_label: String,
    pub left: String,
    pub right: String,
}

/// A request the daemon sends to an attached IDE client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum IdeRequest {
    /// Apply a set of edits to editor buffers.
    ApplyEdit { edit: WorkspaceEdit },
    /// Reveal (scroll to / focus) a location.
    RevealLocation { location: Location },
    /// Show a diff view.
    ShowDiff { request: DiffRequest },
    #[serde(other)]
    Unknown,
}

/// Severity of an editor diagnostic, mirroring the common LSP levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
    #[serde(other)]
    Unknown,
}

/// One editor diagnostic, forwarded from the IDE for context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub path: String,
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The origin of a file excerpt that enters model context. Every excerpt is
/// labeled with exactly one of these so a client can always show where the text
/// came from (Chapter 10, exit criterion 2). Ordered least→most volatile:
/// a committed revision is reproducible; a dirty buffer is the least stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum SourceProvenance {
    /// The bytes as committed at a specific git revision.
    CommittedAt { revision: String },
    /// The current working-tree file on disk.
    Filesystem,
    /// An unsaved editor buffer that diverges from disk.
    UnsavedIdeBuffer,
    /// A patch the agent generated but has not committed.
    GeneratedPatch,
    /// A file inside the agent's isolated worktree.
    AgentWorktree,
    #[serde(other)]
    Unknown,
}

impl SourceProvenance {
    /// The stable, human-facing label rendered in the TUI/IDE trace view, e.g.
    /// `committed@a1b2c3d`, `filesystem`, `unsaved-ide-buffer`,
    /// `generated-patch`, `agent-worktree`.
    pub fn label(&self) -> String {
        match self {
            SourceProvenance::CommittedAt { revision } => format!("committed@{revision}"),
            SourceProvenance::Filesystem => "filesystem".to_string(),
            SourceProvenance::UnsavedIdeBuffer => "unsaved-ide-buffer".to_string(),
            SourceProvenance::GeneratedPatch => "generated-patch".to_string(),
            SourceProvenance::AgentWorktree => "agent-worktree".to_string(),
            SourceProvenance::Unknown => "unknown".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).expect("serialize");
        let parsed: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, parsed);
    }

    #[test]
    fn ide_types_round_trip() {
        round_trip(IdeContextUpdate {
            active_file: Some("src/lib.rs".to_string()),
            selection: Some(EditorSelection {
                path: "src/lib.rs".to_string(),
                range: Range {
                    start: Position {
                        line: 1,
                        character: 0,
                    },
                    end: Position {
                        line: 2,
                        character: 4,
                    },
                },
            }),
            open_files: vec!["src/lib.rs".to_string(), "Cargo.toml".to_string()],
            dirty_buffers: vec![DirtyBufferDigest {
                path: "src/lib.rs".to_string(),
                sha256: "abc123".to_string(),
                byte_length: 42,
            }],
            diagnostics_revision: 7,
        });
        round_trip(IdeRequest::RevealLocation {
            location: Location {
                path: "src/lib.rs".to_string(),
                range: None,
            },
        });
        round_trip(Diagnostic {
            path: "src/lib.rs".to_string(),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types".to_string(),
            source: Some("rustc".to_string()),
        });
        round_trip(SourceProvenance::CommittedAt {
            revision: "a1b2c3d".to_string(),
        });
    }

    #[test]
    fn provenance_labels_are_normative() {
        assert_eq!(
            SourceProvenance::CommittedAt {
                revision: "a1b2c3d".to_string()
            }
            .label(),
            "committed@a1b2c3d"
        );
        assert_eq!(SourceProvenance::Filesystem.label(), "filesystem");
        assert_eq!(
            SourceProvenance::UnsavedIdeBuffer.label(),
            "unsaved-ide-buffer"
        );
        assert_eq!(SourceProvenance::GeneratedPatch.label(), "generated-patch");
        assert_eq!(SourceProvenance::AgentWorktree.label(), "agent-worktree");
    }

    #[test]
    fn unknown_tags_degrade() {
        let future = serde_json::json!({ "type": "FromTheFuture" });
        assert!(matches!(
            serde_json::from_value::<SourceProvenance>(future.clone()).expect("provenance"),
            SourceProvenance::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<IdeRequest>(future.clone()).expect("request"),
            IdeRequest::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<DiagnosticSeverity>(future).expect("severity"),
            DiagnosticSeverity::Unknown
        ));
    }
}
