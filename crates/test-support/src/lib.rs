//! Shared fixtures and helpers for Codypendent tests.

use codypendent_protocol::SessionEvent;

/// Raw JSONL of the basic event fixture (one serialized `SessionEvent` per
/// line). Kept as text so protocol-compatibility tests can replay the exact
/// historical bytes, not merely re-serialized structs.
pub fn fixture_events_jsonl() -> &'static str {
    include_str!("../fixtures/events-basic.jsonl")
}

/// Parse the basic fixture into events. Panics on malformed fixture content —
/// a broken fixture is a bug in the repository, not a runtime condition.
pub fn load_fixture_events() -> Vec<SessionEvent> {
    fixture_events_jsonl()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("fixture line must parse as SessionEvent"))
        .collect()
}
