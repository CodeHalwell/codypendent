//! The common IDE bridge contract (Chapter 10).
//!
//! [`IdeBridge`] is the transport-agnostic surface the daemon depends on: reads
//! of the editor's live state and requests for the editor to act. The IDE
//! applies edits *semantically* in the editor and never executes tools itself
//! (invariant 2), so every mutating direction here is a request, not an action
//! the daemon performs.
//!
//! [`RecordingIdeBridge`] is an in-memory implementation for tests and for the
//! assembly layer to exercise the wiring before a real editor is attached: it
//! returns configurable state and records every request it is asked to perform.

use async_trait::async_trait;
use codypendent_protocol::ide::{
    Diagnostic, DiffRequest, EditorSelection, Location, WorkspaceEdit,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::IdeError;

/// A snapshot of the workspace the IDE has open: its root, the open file paths,
/// and the active file (if any).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Absolute path to the workspace root.
    pub root: String,
    /// Paths of every open file.
    pub open_files: Vec<String>,
    /// The file the user is focused on, if any.
    pub active_file: Option<String>,
}

/// One open editor document: its path, the editor's language identifier, and
/// whether it has unsaved ("dirty") changes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OpenDocument {
    /// The document path.
    pub path: String,
    /// The editor's language identifier (e.g. `rust`, `toml`).
    pub language_id: String,
    /// Whether the buffer has unsaved changes.
    pub dirty: bool,
}

/// The transport-agnostic IDE surface the daemon depends on. Implementations
/// bridge to a concrete editor (VS Code, a language client, …); the daemon never
/// depends on a concrete editor, only on this trait.
#[async_trait]
pub trait IdeBridge: Send + Sync {
    /// The current workspace snapshot.
    async fn workspace_state(&self) -> Result<WorkspaceState, IdeError>;

    /// Every open document, with its language and dirty state.
    async fn open_documents(&self) -> Result<Vec<OpenDocument>, IdeError>;

    /// The editor's current selection, if there is one.
    async fn active_selection(&self) -> Result<Option<EditorSelection>, IdeError>;

    /// The diagnostics the IDE currently holds.
    async fn diagnostics(&self) -> Result<Vec<Diagnostic>, IdeError>;

    /// Ask the IDE to apply a set of edits to its buffers.
    async fn apply_edit(&self, edit: WorkspaceEdit) -> Result<(), IdeError>;

    /// Ask the IDE to reveal (scroll to / focus) a location.
    async fn reveal_location(&self, location: Location) -> Result<(), IdeError>;

    /// Ask the IDE to display a diff view.
    async fn show_diff(&self, request: DiffRequest) -> Result<(), IdeError>;
}

/// An in-memory [`IdeBridge`] that returns configurable state and records every
/// request it is asked to perform, so a test can assert what the daemon asked
/// the editor to do without a real editor attached.
#[derive(Debug, Default)]
pub struct RecordingIdeBridge {
    workspace: Mutex<WorkspaceState>,
    open_documents: Mutex<Vec<OpenDocument>>,
    selection: Mutex<Option<EditorSelection>>,
    diagnostics: Mutex<Vec<Diagnostic>>,
    applied_edits: Mutex<Vec<WorkspaceEdit>>,
    revealed_locations: Mutex<Vec<Location>>,
    shown_diffs: Mutex<Vec<DiffRequest>>,
}

impl RecordingIdeBridge {
    /// Construct a bridge that reports `workspace` and otherwise-empty state.
    pub fn new(workspace: WorkspaceState) -> Self {
        Self {
            workspace: Mutex::new(workspace),
            ..Self::default()
        }
    }

    /// Configure the open documents this bridge reports.
    pub async fn set_open_documents(&self, documents: Vec<OpenDocument>) {
        *self.open_documents.lock().await = documents;
    }

    /// Configure the selection this bridge reports.
    pub async fn set_selection(&self, selection: Option<EditorSelection>) {
        *self.selection.lock().await = selection;
    }

    /// Configure the diagnostics this bridge reports.
    pub async fn set_diagnostics(&self, diagnostics: Vec<Diagnostic>) {
        *self.diagnostics.lock().await = diagnostics;
    }

    /// Every edit that has been applied through this bridge, in order.
    pub async fn applied_edits(&self) -> Vec<WorkspaceEdit> {
        self.applied_edits.lock().await.clone()
    }

    /// Every location that has been revealed through this bridge, in order.
    pub async fn revealed_locations(&self) -> Vec<Location> {
        self.revealed_locations.lock().await.clone()
    }

    /// Every diff that has been shown through this bridge, in order.
    pub async fn shown_diffs(&self) -> Vec<DiffRequest> {
        self.shown_diffs.lock().await.clone()
    }
}

#[async_trait]
impl IdeBridge for RecordingIdeBridge {
    async fn workspace_state(&self) -> Result<WorkspaceState, IdeError> {
        Ok(self.workspace.lock().await.clone())
    }

    async fn open_documents(&self) -> Result<Vec<OpenDocument>, IdeError> {
        Ok(self.open_documents.lock().await.clone())
    }

    async fn active_selection(&self) -> Result<Option<EditorSelection>, IdeError> {
        Ok(self.selection.lock().await.clone())
    }

    async fn diagnostics(&self) -> Result<Vec<Diagnostic>, IdeError> {
        Ok(self.diagnostics.lock().await.clone())
    }

    async fn apply_edit(&self, edit: WorkspaceEdit) -> Result<(), IdeError> {
        self.applied_edits.lock().await.push(edit);
        Ok(())
    }

    async fn reveal_location(&self, location: Location) -> Result<(), IdeError> {
        self.revealed_locations.lock().await.push(location);
        Ok(())
    }

    async fn show_diff(&self, request: DiffRequest) -> Result<(), IdeError> {
        self.shown_diffs.lock().await.push(request);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::ide::{Position, Range, TextEdit};

    fn range() -> Range {
        Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        }
    }

    #[tokio::test]
    async fn records_requests_and_returns_configured_state() {
        let bridge = RecordingIdeBridge::new(WorkspaceState {
            root: "/w".to_string(),
            open_files: vec!["src/lib.rs".to_string()],
            active_file: Some("src/lib.rs".to_string()),
        });

        let state = bridge.workspace_state().await.expect("state");
        assert_eq!(state.active_file.as_deref(), Some("src/lib.rs"));

        bridge
            .apply_edit(WorkspaceEdit {
                edits: vec![TextEdit {
                    path: "src/lib.rs".to_string(),
                    range: range(),
                    new_text: "x".to_string(),
                }],
            })
            .await
            .expect("apply");
        bridge
            .reveal_location(Location {
                path: "src/lib.rs".to_string(),
                range: Some(range()),
            })
            .await
            .expect("reveal");

        assert_eq!(bridge.applied_edits().await.len(), 1);
        assert_eq!(bridge.revealed_locations().await.len(), 1);
        assert!(bridge.shown_diffs().await.is_empty());
    }
}
