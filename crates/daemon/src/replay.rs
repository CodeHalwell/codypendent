//! Projection replay.
//!
//! Projections are derived state: folding the same events must always produce
//! the same projection (tested property). This Phase 0 projection is small;
//! later phases add run, approval, and workflow projections built the same
//! way.

use codypendent_protocol::{EventBody, SessionEvent};
use serde::Serialize;

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct SessionProjection {
    pub title: Option<String>,
    pub note_count: u64,
    pub closed: bool,
    pub last_sequence: u64,
    pub event_count: u64,
}

pub fn project(events: &[SessionEvent]) -> SessionProjection {
    let mut projection = SessionProjection::default();
    for event in events {
        projection.event_count += 1;
        projection.last_sequence = event.sequence;
        match &event.body {
            EventBody::SessionCreated { title } => projection.title = Some(title.clone()),
            EventBody::NoteAppended { .. } => projection.note_count += 1,
            EventBody::SessionClosed => projection.closed = true,
            _ => {}
        }
    }
    projection
}
