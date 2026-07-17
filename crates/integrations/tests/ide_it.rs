//! Integration tests for the IDE bridge contract and source provenance
//! (Phase 3 STEP 3.4), exercising only the crate's public API.
//!
//! The focus is the behavior a real editor client depends on: an unsaved buffer
//! that diverges from disk wins provenance, a burst of context updates collapses
//! to the settled state, and the bridge faithfully records every request the
//! daemon makes.

use codypendent_integrations::ide::{
    coalesce_bursts, resolve_source, IdeBridge, RecordingIdeBridge, WorkspaceState,
};
use codypendent_protocol::ide::{
    DirtyBufferDigest, IdeContextUpdate, Location, Position, Range, SourceProvenance, TextEdit,
    WorkspaceEdit,
};

fn range() -> Range {
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 1,
            character: 0,
        },
    }
}

#[test]
fn provenance_prefers_dirty_buffer() {
    let buffers = [DirtyBufferDigest {
        path: "src/lib.rs".to_string(),
        sha256: "AAAA".to_string(),
        byte_length: 4,
    }];

    // Buffer digest differs from disk -> the unsaved buffer is the truth.
    let diverging = resolve_source("src/lib.rs", &buffers, Some("BBBB"), None, false);
    assert_eq!(diverging, SourceProvenance::UnsavedIdeBuffer);
    assert_eq!(diverging.label(), "unsaved-ide-buffer");

    // Buffer digest equals disk -> not a divergence, falls through to disk.
    let equal = resolve_source("src/lib.rs", &buffers, Some("AAAA"), None, false);
    assert_eq!(equal, SourceProvenance::Filesystem);
}

#[test]
fn debounce_collapses_a_burst() {
    let mut events: Vec<(u64, IdeContextUpdate)> = [0u64, 50, 100, 150, 200]
        .into_iter()
        .map(|ts| {
            (
                ts,
                IdeContextUpdate {
                    diagnostics_revision: ts,
                    ..IdeContextUpdate::default()
                },
            )
        })
        .collect();

    // One burst within the window collapses to a single settled update.
    assert_eq!(coalesce_bursts(&events, 300).len(), 1);

    // A later event beyond the window opens a second burst.
    events.push((
        600,
        IdeContextUpdate {
            diagnostics_revision: 600,
            ..IdeContextUpdate::default()
        },
    ));
    assert_eq!(coalesce_bursts(&events, 300).len(), 2);
}

#[tokio::test]
async fn recording_bridge_records_apply_edit() {
    let bridge = RecordingIdeBridge::new(WorkspaceState {
        root: "/workspace".to_string(),
        open_files: vec!["src/lib.rs".to_string()],
        active_file: Some("src/lib.rs".to_string()),
    });

    let edit = WorkspaceEdit {
        edits: vec![TextEdit {
            path: "src/lib.rs".to_string(),
            range: range(),
            new_text: "// edited\n".to_string(),
        }],
    };
    bridge.apply_edit(edit.clone()).await.expect("apply edit");

    let location = Location {
        path: "src/lib.rs".to_string(),
        range: Some(range()),
    };
    bridge
        .reveal_location(location.clone())
        .await
        .expect("reveal location");

    let applied = bridge.applied_edits().await;
    assert_eq!(applied, vec![edit]);

    let revealed = bridge.revealed_locations().await;
    assert_eq!(revealed, vec![location]);

    // Nothing else was requested.
    assert!(bridge.shown_diffs().await.is_empty());
}
