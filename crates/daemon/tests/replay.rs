//! Phase 0 exit criterion: the daemon can replay a fixture event log, and
//! replay through the ledger produces the same projection as folding the
//! fixture directly (event replay determinism, from the testing strategy).

use codypendent_daemon::{db, ledger, replay};
use codypendent_protocol::SessionId;

#[tokio::test]
async fn fixture_replay_produces_expected_projection() {
    let events = codypendent_test_support::load_fixture_events();
    let direct = replay::project(&events);
    assert_eq!(direct.title.as_deref(), Some("fixture session"));
    assert_eq!(direct.note_count, 2);
    assert!(direct.closed);
    assert_eq!(direct.last_sequence, 4);
    assert_eq!(direct.event_count, 4);

    let tmp = tempfile::tempdir().expect("create temp dir");
    let pool = db::open_database(&tmp.path().join("codypendent.db"))
        .await
        .expect("open db");
    let session_id = SessionId::new();
    ledger::create_session(&pool, session_id, "fixture session")
        .await
        .expect("create session");
    for event in &events {
        ledger::append_event(&pool, session_id, event)
            .await
            .expect("append event");
    }

    let loaded = ledger::load_events(&pool, session_id)
        .await
        .expect("load events");
    assert_eq!(
        loaded, events,
        "ledger round-trip must preserve events exactly"
    );
    assert_eq!(
        replay::project(&loaded),
        direct,
        "replay must be deterministic"
    );

    let next = ledger::next_sequence(&pool, session_id)
        .await
        .expect("next sequence");
    assert_eq!(next, 5);
}

#[tokio::test]
async fn duplicate_sequence_is_rejected() {
    let events = codypendent_test_support::load_fixture_events();
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pool = db::open_database(&tmp.path().join("codypendent.db"))
        .await
        .expect("open db");
    let session_id = SessionId::new();
    ledger::create_session(&pool, session_id, "fixture session")
        .await
        .expect("create session");
    ledger::append_event(&pool, session_id, &events[0])
        .await
        .expect("first append succeeds");
    let duplicate = ledger::append_event(&pool, session_id, &events[0]).await;
    assert!(
        duplicate.is_err(),
        "appending the same sequence twice must fail"
    );
}
