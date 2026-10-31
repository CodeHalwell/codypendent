//! Production-composition proof for council child-session lifecycle ownership.

use std::path::Path;
use std::str::FromStr as _;
use std::time::Duration;

use codypendent_council::{CouncilDefinition, CouncilMember, CouncilService, FileCouncilService};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    Actor, Catchup, ClientRole, CommandBody, EventBody, Payload, RunDisposition, RunId,
    SessionEvent, SessionId, Subscription,
};
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> AbortOnDrop<T> {
    fn new(task: tokio::task::JoinHandle<T>) -> Self {
        Self(Some(task))
    }

    fn take(&mut self) -> tokio::task::JoinHandle<T> {
        self.0.take().expect("task already taken")
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

fn assert_closed_child_evidence(events: &[SessionEvent], run_ids: &[RunId]) {
    let closed = events
        .iter()
        .filter(|event| matches!(event.body, EventBody::SessionClosed))
        .collect::<Vec<_>>();
    assert_eq!(closed.len(), 1);
    if run_ids.is_empty() {
        assert!(events.iter().all(|event| !matches!(
            event.body,
            EventBody::RunStarted { .. } | EventBody::RunCompleted { .. }
        )));
    }
    for run_id in run_ids {
        let completed = events
            .iter()
            .find(|event| matches!(event.body, EventBody::RunCompleted { run_id: actual, .. } if actual == *run_id))
            .expect("terminal evidence for each persisted child run");
        assert!(completed.sequence < closed[0].sequence);
    }
}

async fn model_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind model fixture");
    let address = listener.local_addr().expect("fixture address");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream
                        .read(&mut buffer)
                        .await
                        .expect("read fixture request");
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let Some(head_end) =
                        request.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let head_end = head_end + 4;
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + content_length {
                        break;
                    }
                }

                let request_text = String::from_utf8_lossy(&request);
                let first_line = request_text.lines().next().unwrap_or_default().to_owned();
                let (status, content_type, body) = if first_line.starts_with("GET /v1/models ") {
                    (
                        "200 OK",
                        "application/json",
                        serde_json::json!({
                            "data": [
                                {"id": "member-a-model"},
                                {"id": "member-b-model"},
                                {"id": "member-fail-model"},
                                {"id": "member-hang-model"},
                                {"id": "chair-fail-model"},
                                {"id": "chair-model"}
                            ]
                        })
                        .to_string(),
                    )
                } else if request_text.contains("-fail-model") {
                    (
                        "400 Bad Request",
                        "application/json",
                        serde_json::json!({"error": {"message": "injected terminal model failure"}})
                            .to_string(),
                    )
                } else if request_text.contains("member-hang-model") {
                    std::future::pending::<()>().await;
                    unreachable!("hanging model fixture never responds")
                } else {
                    (
                        "200 OK",
                        "text/event-stream",
                        concat!(
                            "data: {\"id\":\"fixture-response\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"fixture response\"},\"finish_reason\":null}]}\n\n",
                            "data: {\"id\":\"fixture-response\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
                            "data: [DONE]\n\n"
                        )
                        .to_owned(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(), body,
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write fixture response");
            });
        }
    });
    (format!("http://{address}/v1"), task)
}

async fn control(socket: &Path, payload: Payload) -> Option<Payload> {
    use codypendent_protocol::{read_envelope, write_envelope, ClientId, Envelope};
    let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    write_envelope(&mut stream, &Envelope::request(ClientId::new(), payload))
        .await
        .ok()?;
    Some(read_envelope(&mut stream).await.ok()??.payload)
}

#[tokio::test]
async fn successful_council_closes_real_member_and_chair_sessions_after_terminal_evidence() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repository = tmp.path().join("repository");
    std::fs::create_dir(&repository).expect("repository");
    let paths = RuntimePaths::from_data_dir(tmp.path().join("data"));
    paths.ensure_directories().expect("runtime directories");
    let (base_url, fixture) = model_server().await;
    let _fixture = AbortOnDrop::new(fixture);
    std::fs::write(
        paths.data_dir.join("models.toml"),
        format!(
            r#"
[[model]]
id = "member-a"
provider = "openai-compatible"
base_url = "{base_url}"
model = "member-a-model"
api_key_env = ""

[[model]]
id = "member-b"
provider = "openai-compatible"
base_url = "{base_url}"
model = "member-b-model"
api_key_env = ""

[[model]]
id = "chair"
provider = "openai-compatible"
base_url = "{base_url}"
model = "chair-model"
api_key_env = ""

[[model]]
id = "member-fail"
provider = "openai-compatible"
base_url = "{base_url}"
model = "member-fail-model"
api_key_env = ""

[[model]]
id = "chair-fail"
provider = "openai-compatible"
base_url = "{base_url}"
model = "chair-fail-model"
api_key_env = ""

[[model]]
id = "member-hang"
provider = "openai-compatible"
base_url = "{base_url}"
model = "member-hang-model"
api_key_env = ""
"#
        ),
    )
    .expect("models config");

    let mut daemon = AbortOnDrop::new(tokio::spawn({
        let paths = paths.clone();
        async move { codypendent_codypendentd::run_daemon(paths).await }
    }));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                control(&paths.socket_path, Payload::Ping).await,
                Some(Payload::Pong)
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon ready");

    let service = FileCouncilService::new(paths.clone());
    service
        .create(CouncilDefinition {
            name: "lifecycle".to_owned(),
            description: "integration fixture".to_owned(),
            chair: "chair".to_owned(),
            rounds: 1,
            quorum: Some(2),
            evidence: false,
            review: false,
            reviewer: None,
            members: vec![
                CouncilMember {
                    model: "member-a".to_owned(),
                    role: "reviewer-a".to_owned(),
                },
                CouncilMember {
                    model: "member-b".to_owned(),
                    role: "reviewer-b".to_owned(),
                },
            ],
        })
        .await
        .expect("persist council");
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        service.run(
            "lifecycle",
            "Review the lifecycle fixture".to_owned(),
            repository,
            None,
            false,
        ),
    )
    .await
    .expect("council deadline")
    .expect("successful council");
    assert_eq!(outcome.outcome.members.len(), 2);

    let pool = SqlitePool::connect(&format!(
        "sqlite://{}",
        paths.data_dir.join("codypendent.db").display()
    ))
    .await
    .expect("open daemon database");
    let mut child_ids = outcome
        .outcome
        .members
        .iter()
        .map(|member| member.session_id)
        .collect::<Vec<_>>();
    child_ids.push(outcome.outcome.chair.session_id);
    let replay_session = child_ids[0];
    for session_id in child_ids {
        let (state,): (String,) = sqlx::query_as("SELECT state FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("child session");
        assert_eq!(state, "closed");
        let events = codypendent_daemon::ledger::load_events(&pool, session_id)
            .await
            .expect("closed child history");
        let completed = events
            .iter()
            .find(|event| matches!(event.body, EventBody::RunCompleted { .. }))
            .expect("terminal evidence");
        let closed = events
            .iter()
            .filter(|event| matches!(event.body, EventBody::SessionClosed))
            .collect::<Vec<_>>();
        assert_eq!(closed.len(), 1, "exactly one closure event");
        assert!(completed.sequence < closed[0].sequence);
        assert!(matches!(completed.actor, Actor::Agent { .. }));
        assert!(matches!(closed[0].actor, Actor::Client { .. }));
        assert!(matches!(
            events.last().map(|event| &event.body),
            Some(EventBody::SessionClosed)
        ));
    }
    let mut replay = codypendent_council::connection::Connection::connect(&paths.socket_path)
        .await
        .expect("reconnect to daemon");
    replay
        .handshake("council-lifecycle-test", env!("CARGO_PKG_VERSION"), None)
        .await
        .expect("reconnect handshake");
    let catchup = replay
        .send_command(CommandBody::AttachSession {
            session_id: replay_session,
            last_seen_sequence: Some(0),
            subscriptions: vec![Subscription::SessionSummary, Subscription::AgentActivity],
            requested_role: ClientRole::Observer,
            repository: None,
        })
        .await
        .expect("attach closed council child");
    let Payload::Catchup {
        catchup: Catchup::Events { events, .. },
    } = catchup.payload
    else {
        panic!("closed council history was not replayed as events");
    };
    assert!(events
        .iter()
        .any(|event| matches!(event.body, EventBody::RunCompleted { .. })));
    assert!(matches!(
        events.last().map(|event| &event.body),
        Some(EventBody::SessionClosed)
    ));

    let prior_ids =
        sqlx::query_as::<_, (String,)>("SELECT id FROM sessions WHERE title LIKE 'Council · %'")
            .fetch_all(&pool)
            .await
            .expect("successful child ids")
            .into_iter()
            .map(|(id,)| id)
            .collect::<std::collections::HashSet<_>>();
    service
        .create(CouncilDefinition {
            name: "member-failure".to_owned(),
            description: String::new(),
            chair: "chair".to_owned(),
            rounds: 1,
            quorum: Some(2),
            evidence: false,
            review: false,
            reviewer: None,
            members: vec![
                CouncilMember {
                    model: "member-a".to_owned(),
                    role: "reviewer-a".to_owned(),
                },
                CouncilMember {
                    model: "member-b".to_owned(),
                    role: "reviewer-b".to_owned(),
                },
                CouncilMember {
                    model: "member-fail".to_owned(),
                    role: "terminal-failure".to_owned(),
                },
            ],
        })
        .await
        .expect("member failure council");
    tokio::time::timeout(
        Duration::from_secs(30),
        service.run(
            "member-failure",
            "Retain successful work despite one terminal member failure".to_owned(),
            tmp.path().join("repository"),
            None,
            false,
        ),
    )
    .await
    .expect("member failure deadline")
    .expect("quorum permits synthesis");
    let member_failure_children: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, title, state FROM sessions WHERE title LIKE 'Council · %'")
            .fetch_all(&pool)
            .await
            .expect("member failure children")
            .into_iter()
            .filter(|(id, _, _)| !prior_ids.contains(id))
            .collect();
    assert_eq!(member_failure_children.len(), 4);
    assert!(member_failure_children
        .iter()
        .all(|(_, _, state)| state == "closed"));
    for (id, _, _) in &member_failure_children {
        let events = codypendent_daemon::ledger::load_events(
            &pool,
            SessionId::from_str(id).expect("session id"),
        )
        .await
        .expect("member-failure child history");
        let completed = events
            .iter()
            .find(|event| matches!(event.body, EventBody::RunCompleted { .. }))
            .expect("terminal evidence");
        let closed = events
            .iter()
            .filter(|event| matches!(event.body, EventBody::SessionClosed))
            .collect::<Vec<_>>();
        assert_eq!(closed.len(), 1);
        assert!(completed.sequence < closed[0].sequence);
    }
    let failed_member = member_failure_children
        .iter()
        .find(|(_, title, _)| title.contains("member-fail"))
        .expect("failed member child");
    let failed_events = codypendent_daemon::ledger::load_events(
        &pool,
        SessionId::from_str(&failed_member.0).expect("session id"),
    )
    .await
    .expect("failed member history");
    assert!(failed_events.iter().any(|event| matches!(
        event.body,
        EventBody::RunCompleted {
            disposition: RunDisposition::Failed { .. },
            ..
        }
    )));
    assert!(matches!(
        failed_events.last().map(|event| &event.body),
        Some(EventBody::SessionClosed)
    ));
    assert!(matches!(
        failed_events.last().map(|event| &event.actor),
        Some(Actor::Client { .. })
    ));

    let prior_ids =
        sqlx::query_as::<_, (String,)>("SELECT id FROM sessions WHERE title LIKE 'Council · %'")
            .fetch_all(&pool)
            .await
            .expect("pre-chair-failure child ids")
            .into_iter()
            .map(|(id,)| id)
            .collect::<std::collections::HashSet<_>>();
    service
        .create(CouncilDefinition {
            name: "chair-failure".to_owned(),
            description: String::new(),
            chair: "chair-fail".to_owned(),
            rounds: 1,
            quorum: Some(2),
            evidence: false,
            review: false,
            reviewer: None,
            members: vec![
                CouncilMember {
                    model: "member-a".to_owned(),
                    role: "reviewer-a".to_owned(),
                },
                CouncilMember {
                    model: "member-b".to_owned(),
                    role: "reviewer-b".to_owned(),
                },
            ],
        })
        .await
        .expect("chair failure council");
    tokio::time::timeout(
        Duration::from_secs(30),
        service.run(
            "chair-failure",
            "Preserve members when the chair fails".to_owned(),
            tmp.path().join("repository"),
            None,
            false,
        ),
    )
    .await
    .expect("chair failure deadline")
    .expect_err("terminal chair failure is surfaced");
    let chair_failure_children: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, title, state FROM sessions WHERE title LIKE 'Council · %'")
            .fetch_all(&pool)
            .await
            .expect("chair failure children")
            .into_iter()
            .filter(|(id, _, _)| !prior_ids.contains(id))
            .collect();
    assert_eq!(chair_failure_children.len(), 3);
    assert!(chair_failure_children
        .iter()
        .all(|(_, _, state)| state == "closed"));
    for (id, _, _) in &chair_failure_children {
        let events = codypendent_daemon::ledger::load_events(
            &pool,
            SessionId::from_str(id).expect("session id"),
        )
        .await
        .expect("chair-failure child history");
        let completed = events
            .iter()
            .find(|event| matches!(event.body, EventBody::RunCompleted { .. }))
            .expect("terminal evidence");
        let closed = events
            .iter()
            .filter(|event| matches!(event.body, EventBody::SessionClosed))
            .collect::<Vec<_>>();
        assert_eq!(closed.len(), 1);
        assert!(completed.sequence < closed[0].sequence);
    }
    let failed_chair = chair_failure_children
        .iter()
        .find(|(_, title, _)| title.contains("chair-fail"))
        .expect("failed chair child");
    let failed_events = codypendent_daemon::ledger::load_events(
        &pool,
        SessionId::from_str(&failed_chair.0).expect("session id"),
    )
    .await
    .expect("failed chair history");
    assert!(failed_events.iter().any(|event| matches!(
        event.body,
        EventBody::RunCompleted {
            disposition: RunDisposition::Failed { .. },
            ..
        }
    )));
    assert!(matches!(
        failed_events.last().map(|event| &event.body),
        Some(EventBody::SessionClosed)
    ));
    assert!(matches!(
        failed_events.last().map(|event| &event.actor),
        Some(Actor::Client { .. })
    ));

    let prior_ids =
        sqlx::query_as::<_, (String,)>("SELECT id FROM sessions WHERE title LIKE 'Council · %'")
            .fetch_all(&pool)
            .await
            .expect("pre-abort child ids")
            .into_iter()
            .map(|(id,)| id)
            .collect::<std::collections::HashSet<_>>();
    service
        .create(CouncilDefinition {
            name: "parent-abort".to_owned(),
            description: String::new(),
            chair: "chair".to_owned(),
            rounds: 1,
            quorum: Some(2),
            evidence: false,
            review: false,
            reviewer: None,
            members: vec![
                CouncilMember {
                    model: "member-a".to_owned(),
                    role: "fast-member".to_owned(),
                },
                CouncilMember {
                    model: "member-hang".to_owned(),
                    role: "active-member".to_owned(),
                },
            ],
        })
        .await
        .expect("abort council");
    let aborted = tokio::spawn({
        let service = service.clone();
        let repository = tmp.path().join("repository");
        async move {
            service
                .run(
                    "parent-abort",
                    "Abort while one child is active".to_owned(),
                    repository,
                    None,
                    false,
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let active: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM runs r JOIN sessions s ON s.id = r.session_id \
                 WHERE s.title LIKE '%member-hang%' \
                   AND r.state NOT IN ('Completed', 'Failed', 'Cancelled')",
            )
            .fetch_one(&pool)
            .await
            .expect("active child count");
            let children: Vec<(String,)> =
                sqlx::query_as("SELECT id FROM sessions WHERE title LIKE 'Council · %'")
                    .fetch_all(&pool)
                    .await
                    .expect("reserved child sessions");
            let reserved = children
                .iter()
                .filter(|(id,)| !prior_ids.contains(id))
                .count();
            if active.0 > 0 && reserved == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("both child sessions were reserved and the hanging child became active");
    aborted.abort();
    let _ = aborted.await;
    let aborted_children = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let rows: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT id, title, state FROM sessions WHERE title LIKE 'Council · %'",
            )
            .fetch_all(&pool)
            .await
            .expect("aborted children");
            let created = rows
                .into_iter()
                .filter(|(id, _, _)| !prior_ids.contains(id))
                .collect::<Vec<_>>();
            if created.len() == 2 && created.iter().all(|(_, _, state)| state == "closed") {
                break created;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let aborted_children = match aborted_children {
        Ok(children) => children,
        Err(_) => {
            let sessions: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT id, title, state FROM sessions WHERE title LIKE 'Council · %'",
            )
            .fetch_all(&pool)
            .await
            .expect("diagnose aborted sessions")
            .into_iter()
            .filter(|(id, _, _)| !prior_ids.contains(id))
            .collect();
            let runs: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT s.title, r.id, r.state FROM runs r JOIN sessions s ON s.id = r.session_id \
                 WHERE s.title LIKE 'Council · %' ORDER BY s.title, r.id",
            )
            .fetch_all(&pool)
            .await
            .expect("diagnose aborted runs");
            let events: Vec<(String, i64, String)> = sqlx::query_as(
                "SELECT e.session_id, e.sequence, json_extract(e.body, '$.type') FROM events e \
                 JOIN sessions s ON s.id = e.session_id \
                 WHERE s.title LIKE 'Council · %' ORDER BY e.session_id, e.sequence",
            )
            .fetch_all(&pool)
            .await
            .expect("diagnose aborted events")
            .into_iter()
            .filter(|(session_id, _, _)| !prior_ids.contains(session_id))
            .collect();
            panic!(
                "drop cleanup did not close aborted children; sessions={sessions:?}; runs={runs:?}; events={events:?}"
            );
        }
    };
    let hanging_child = aborted_children
        .iter()
        .find(|(_, title, _)| title.contains("member-hang"))
        .expect("hanging child session");
    for (id, _, _) in &aborted_children {
        let events = codypendent_daemon::ledger::load_events(
            &pool,
            SessionId::from_str(id).expect("session id"),
        )
        .await
        .expect("aborted child history");
        let run_ids = sqlx::query_as::<_, (String,)>("SELECT id FROM runs WHERE session_id = ?")
            .bind(id)
            .fetch_all(&pool)
            .await
            .expect("aborted child runs")
            .into_iter()
            .map(|(id,)| RunId::from_str(&id).expect("run id"))
            .collect::<Vec<_>>();
        assert_closed_child_evidence(&events, &run_ids);
    }
    let hanging_events = codypendent_daemon::ledger::load_events(
        &pool,
        SessionId::from_str(&hanging_child.0).expect("session id"),
    )
    .await
    .expect("hanging child history");
    let completed = hanging_events
        .iter()
        .find(|event| matches!(event.body, EventBody::RunCompleted { .. }))
        .expect("aborted child has terminal evidence");
    let closed = hanging_events
        .iter()
        .find(|event| matches!(event.body, EventBody::SessionClosed))
        .expect("aborted child is closed");
    assert!(completed.sequence < closed.sequence);
    assert!(matches!(closed.actor, Actor::Client { .. }));
    assert!(matches!(
        hanging_events.last().map(|event| &event.body),
        Some(EventBody::SessionClosed)
    ));

    assert!(matches!(
        control(&paths.socket_path, Payload::Shutdown).await,
        Some(Payload::ShutdownAck)
    ));
    tokio::time::timeout(Duration::from_secs(10), daemon.take())
        .await
        .expect("daemon shutdown")
        .expect("daemon task")
        .expect("daemon result");
}

#[cfg(test)]
mod child_evidence_tests {
    use super::*;
    use codypendent_protocol::{ArtifactId, ArtifactRef, DataClassification};

    fn event(sequence: u64, body: EventBody) -> SessionEvent {
        SessionEvent {
            sequence,
            occurred_at: "2026-10-31T00:00:00Z".parse().unwrap(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body,
        }
    }

    fn completed(run_id: RunId) -> SessionEvent {
        event(
            1,
            EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Cancelled { reason: None },
                chronicle: ArtifactRef {
                    id: ArtifactId::new(),
                    media_type: "application/json".into(),
                    byte_length: 0,
                    sha256: "0".repeat(64),
                    sensitivity: DataClassification::Internal,
                },
            },
        )
    }

    #[test]
    fn unused_closed_child_has_no_run_completion() {
        assert_closed_child_evidence(&[event(1, EventBody::SessionClosed)], &[]);
    }

    #[test]
    fn launched_child_has_terminal_evidence_before_closure() {
        let run = RunId::new();
        assert_closed_child_evidence(
            &[completed(run), event(2, EventBody::SessionClosed)],
            &[run],
        );
    }

    #[test]
    #[should_panic(expected = "terminal evidence for each persisted child run")]
    fn another_runs_completion_cannot_cover_a_persisted_child_run() {
        let first = RunId::new();
        assert_closed_child_evidence(
            &[completed(first), event(2, EventBody::SessionClosed)],
            &[first, RunId::new()],
        );
    }
}
