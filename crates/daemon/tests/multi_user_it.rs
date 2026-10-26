//! Outcome 19: the connection principal, session ownership, and approval
//! authority — driven over a real Unix socket against a real daemon.
//!
//! Every test here reproduces something the 2026-08-13 daemon-core review proved
//! on the wire against the shipped daemon (`docs/reviews/2026-08-13-verticals/
//! daemon-core.md`, F-19-1 and F-19-5): a fresh, never-attached client read
//! another session's entire history, and approved another client's parked
//! `shell.run ls -la`, which then executed.
//!
//! The "other user" is a session row whose `owner_uid` is not this process's
//! uid. That is exactly the state the daemon would be in with a second OS user
//! on the machine, and it needs no privilege to set up — which matters, because
//! CI runs everything as one user.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use codypendent_daemon::documents::{
    DocumentLeaseFuture, DocumentLeaseReleaseRequest, DocumentLeaseRequest, DocumentLeaser,
    DocumentPublisher, DocumentReleaseFuture, PublishDocumentFuture, PublishDocumentRequest,
    PublishParked,
};
use codypendent_daemon::executor::{RunExecutor, RunLaunch};
use codypendent_daemon::{db, instance, server};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, Actor, ApprovalDecision, ApprovalId, ApprovalScope, ArtifactId,
    ArtifactRef, ClientCapabilities, ClientHello, ClientId, ClientRole, Command, CommandBody,
    CommandId, DataClassification, DocumentId, Envelope, EventBody, Payload, RunDisposition, RunId,
    SessionId, Subscription, PROTOCOL_V1,
};
use sqlx::SqlitePool;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

type ServerTask = JoinHandle<anyhow::Result<()>>;

async fn stop_server(task: ServerTask) {
    task.abort();
    match task.await {
        Err(error) if error.is_cancelled() => {}
        Ok(Ok(())) => {}
        other => panic!("daemon did not stop cleanly: {other:?}"),
    }
}

async fn start_server(tmp: &tempfile::TempDir) -> (RuntimePaths, ServerTask, SqlitePool) {
    let (paths, task, pool, _) = start_server_with_documents(tmp).await;
    (paths, task, pool)
}

/// Boot with the document seams **wired**, recording what reaches them.
///
/// The executor-less server rejects every document command
/// `document.transport-unavailable`, which would make an ownership test
/// vacuous — it would pass whether or not the gate exists. These tests need to
/// see that a foreign principal's command never reaches the seam that would
/// have parked a Git write or dropped somebody's lease.
async fn start_server_with_documents(
    tmp: &tempfile::TempDir,
) -> (RuntimePaths, ServerTask, SqlitePool, RecordingDocuments) {
    let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
    paths.ensure_directories().expect("create directories");
    let pool = db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db");
    let boot = instance::record_boot(&pool).await.expect("record boot");
    let seed_pool = db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open seed pool");
    let documents = RecordingDocuments::default();
    let executor: Arc<dyn RunExecutor> = Arc::new(documents.clone());
    let mut task = tokio::spawn(server::run_with_executor(
        pool,
        paths.clone(),
        boot,
        Some(executor),
    ));
    // Binding the socket happens before the daemon's startup database work.
    // Complete a real handshake before tests seed through their second pool so
    // those writes cannot race adoption/index maintenance and fail SQLITE_BUSY.
    let mut readiness_stream = connect(&paths, &mut task).await;
    handshake(&mut readiness_stream, ClientId::new()).await;
    drop(readiness_stream);
    (paths, task, seed_pool, documents)
}

/// A [`RunExecutor`] whose document seams record every request instead of
/// performing it — so a test can assert that a refused command reached none.
#[derive(Clone, Default)]
struct RecordingDocuments {
    published: Arc<Mutex<Vec<DocumentId>>>,
    released: Arc<Mutex<Vec<String>>>,
}

impl RunExecutor for RecordingDocuments {
    fn spawn_run(&self, _launch: RunLaunch) {}

    fn document_publisher(&self) -> Option<Arc<dyn DocumentPublisher>> {
        Some(Arc::new(self.clone()))
    }

    fn document_leaser(&self) -> Option<Arc<dyn DocumentLeaser>> {
        Some(Arc::new(self.clone()))
    }
}

impl DocumentPublisher for RecordingDocuments {
    fn publish(&self, request: PublishDocumentRequest) -> PublishDocumentFuture<'_> {
        self.published
            .lock()
            .expect("published lock")
            .push(request.document_id);
        Box::pin(async move {
            Ok(PublishParked {
                approval_id: ApprovalId::new(),
                target_description: "repository file docs/attacker-chose-this.md".to_string(),
                changed_files: vec!["docs/attacker-chose-this.md".to_string()],
                git_action: "write docs/attacker-chose-this.md in the working tree".to_string(),
            })
        })
    }
}

impl DocumentLeaser for RecordingDocuments {
    fn acquire(&self, _request: DocumentLeaseRequest) -> DocumentLeaseFuture<'_> {
        Box::pin(async move {
            Err(codypendent_protocol::CodypendentError::new(
                "document.range-leased",
                "not used by these tests",
                false,
            ))
        })
    }

    fn release(&self, request: DocumentLeaseReleaseRequest) -> DocumentReleaseFuture<'_> {
        self.released
            .lock()
            .expect("released lock")
            .push(request.lease_id);
        Box::pin(async move { Ok(()) })
    }
}

async fn connect(paths: &RuntimePaths, task: &mut ServerTask) -> UnixStream {
    // Starting all twelve real-daemon cases in parallel can leave one server
    // behind the loaded CI runner's scheduler for more than four seconds. Use
    // the same 30-second hang-detector budget as reads below: this remains a
    // readiness wait, not a latency assertion, and returns immediately once
    // the socket is accepting connections.
    for _ in 0..1_500 {
        if task.is_finished() {
            let result = (&mut *task).await;
            panic!("daemon exited before accepting a connection: {result:?}");
        }
        if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon never accepted a connection");
}

async fn read_frame(stream: &mut UnixStream) -> Envelope {
    // A HANG DETECTOR, not a latency assertion: when the daemon answers — which
    // on an idle machine is immediate — this bound costs nothing. Five seconds
    // was comfortable locally and too tight on a loaded CI runner, which is how
    // the v0.12.1 release gate went red on a socket read minutes after the same
    // commit passed the identical command in `ci`.
    tokio::time::timeout(Duration::from_secs(30), read_envelope(stream))
        .await
        .expect("read timed out")
        .expect("read frame")
        .expect("server must reply")
}

/// Send one request and return its reply, skipping the frames an attached
/// connection also receives asynchronously (heartbeat pings, and the presence
/// events an attach itself fans back out).
async fn send_recv(stream: &mut UnixStream, request: &Envelope) -> Envelope {
    write_envelope(stream, request).await.expect("write frame");
    for _ in 0..16 {
        let frame = read_frame(stream).await;
        if !matches!(frame.payload, Payload::Ping | Payload::Event(_)) {
            return frame;
        }
    }
    panic!("no reply arrived among the streamed frames");
}

fn command(body: CommandBody, key: &str) -> Command {
    Command {
        command_id: CommandId::new(),
        idempotency_key: key.to_string(),
        expected_revision: None,
        body,
    }
}

async fn handshake(stream: &mut UnixStream, client_id: ClientId) {
    let hello = ClientHello {
        client_name: "multi-user-it".to_string(),
        client_version: "0.0.0".to_string(),
        supported_protocols: vec![PROTOCOL_V1],
        capabilities: ClientCapabilities::default(),
        resume_token: None,
    };
    let reply = send_recv(
        stream,
        &Envelope::request(client_id, Payload::ClientHello(hello)),
    )
    .await;
    assert!(
        matches!(reply.payload, Payload::ServerHello(_)),
        "handshake must succeed: {:?}",
        reply.payload
    );
}

/// This process's uid, taken from a file it creates — the same trick the daemon
/// uses on its own socket, and the only way to learn it without `libc`.
fn our_uid(tmp: &tempfile::TempDir) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    let probe = tmp.path().join(".uid-probe");
    std::fs::write(&probe, b"x").expect("write probe");
    std::fs::metadata(&probe).expect("stat probe").uid()
}

/// Seed a session owned by somebody else, with one event, one run, and one
/// pending approval on that run — the exact shape the review approved across.
/// Returns `(session, run, approval)`.
async fn seed_foreign_session(
    pool: &SqlitePool,
    foreign_uid: u32,
) -> (SessionId, RunId, ApprovalId) {
    let session_id = SessionId::new();
    let run_id = RunId::new();
    let approval_id = ApprovalId::new();
    let now = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO sessions (id, title, state, created_at, updated_at, revision, owner_uid) \
         VALUES (?, 'another user''s work', 'open', ?, ?, 0, ?)",
    )
    .bind(session_id.to_string())
    .bind(&now)
    .bind(&now)
    .bind(i64::from(foreign_uid))
    .execute(pool)
    .await
    .expect("seed session");

    sqlx::query(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         VALUES (?, 1, ?, ?, ?, NULL, NULL, 1)",
    )
    .bind(session_id.to_string())
    .bind(&now)
    .bind(serde_json::to_string(&codypendent_protocol::Actor::System).unwrap())
    .bind(
        serde_json::to_string(&EventBody::NoteAppended {
            // Stands in for the context manifest the review exfiltrated.
            text: "SECRET: another user's private context".to_string(),
            run_id: None,
        })
        .unwrap(),
    )
    .execute(pool)
    .await
    .expect("seed event");

    sqlx::query(
        "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
         VALUES (?, ?, 'their run', 'AwaitingApproval', 'Build', 'hosted-default', '{}')",
    )
    .bind(run_id.to_string())
    .bind(session_id.to_string())
    .execute(pool)
    .await
    .expect("seed run");

    sqlx::query(
        "INSERT INTO approvals \
         (id, run_id, action_json, risk_json, capabilities_json, state, scope, requested_at) \
         VALUES (?, ?, ?, ?, '[]', 'pending', 'once', ?)",
    )
    .bind(approval_id.to_string())
    .bind(run_id.to_string())
    .bind(r#"{"type":"RunCommand","command":"ls -la"}"#)
    .bind(r#"{"level":"Low"}"#)
    .bind(&now)
    .execute(pool)
    .await
    .expect("seed approval");

    (session_id, run_id, approval_id)
}

async fn approval_state(pool: &SqlitePool, approval_id: ApprovalId) -> (String, Option<String>) {
    sqlx::query_as("SELECT state, resolved_by FROM approvals WHERE id = ?")
        .bind(approval_id.to_string())
        .fetch_one(pool)
        .await
        .expect("read approval")
}

// --- the two attacks the review demonstrated ---------------------------------

/// F-19-5, reproduced and now refused. The review connected a fresh client that
/// had never attached to anything and read all 17 events of somebody else's
/// session, "including the full context manifest … and the model's output".
#[tokio::test]
async fn a_stranger_cannot_read_another_principals_session_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let foreign_uid = our_uid(&tmp) + 1;
    let (paths, mut task, pool) = start_server(&tmp).await;
    let (session_id, _, _) = seed_foreign_session(&pool, foreign_uid).await;

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ReadSessionEvents {
                    session_id,
                    after_sequence: 0,
                    limit: 0,
                },
                "read-foreign",
            )),
        ),
    )
    .await;

    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "protocol.session-not-found");
        }
        Payload::SessionEventsPage { events, .. } => panic!(
            "THE REVIEW'S DISCLOSURE: a stranger read {} events of another principal's session",
            events.len()
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }

    stop_server(task).await;
}

#[tokio::test]
async fn knowledge_cannot_enumerate_a_foreign_or_missing_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let foreign_uid = our_uid(&tmp) + 1;
    let (paths, mut task, pool) = start_server(&tmp).await;
    let (foreign, _, _) = seed_foreign_session(&pool, foreign_uid).await;
    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;
    let mut replies = Vec::new();
    for session_id in [foreign, SessionId::new()] {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::ReadKnowledge {
                        session_id,
                        section: codypendent_protocol::knowledge::KnowledgeSection::Memories,
                        offset: 0,
                        limit: 50,
                    },
                    &format!("knowledge-{session_id}"),
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => replies.push(error.code),
            other => panic!("knowledge disclosure: {other:?}"),
        }
    }
    assert_eq!(
        replies,
        vec!["protocol.session-not-found", "protocol.session-not-found"]
    );
    stop_server(task).await;
}

/// F-19-1, the approval-gate bypass, reproduced and now refused. The review
/// parked a `shell.run ls -la`, resolved it "by a *stranger* client over a raw
/// socket", and the daemon executed it. The gate in front of arbitrary command
/// execution has to hold against a principal that does not own the run.
#[tokio::test]
async fn a_stranger_cannot_resolve_another_principals_approval() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let foreign_uid = our_uid(&tmp) + 1;
    let (paths, mut task, pool) = start_server(&tmp).await;
    let (_, _, approval_id) = seed_foreign_session(&pool, foreign_uid).await;

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ResolveApproval {
                    approval_id,
                    decision: ApprovalDecision::Approve,
                    scope: ApprovalScope::Once,
                },
                "approve-foreign",
            )),
        ),
    )
    .await;

    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "approval.not-found"),
        Payload::CommandAccepted { .. } => {
            panic!("THE REVIEW'S BYPASS: a stranger approved another principal's parked command")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // The refusal has to be real, not merely a different reply: the durable row
    // must still be pending, or the parked runtime waiter would still be woken.
    let (state, resolved_by) = approval_state(&pool, approval_id).await;
    assert_eq!(state, "pending", "the approval must remain unresolved");
    assert_eq!(resolved_by, None);

    stop_server(task).await;
}

/// Attach is the third door onto the same session. It has to answer exactly as a
/// missing session does.
#[tokio::test]
async fn a_stranger_cannot_attach_to_another_principals_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let foreign_uid = our_uid(&tmp) + 1;
    let (paths, mut task, pool) = start_server(&tmp).await;
    let (session_id, _, _) = seed_foreign_session(&pool, foreign_uid).await;

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AttachSession {
                    session_id,
                    last_seen_sequence: None,
                    subscriptions: vec![Subscription::SessionSummary],
                    // Asserting the most privileged role must not help: the role
                    // is a self-restriction, never a grant.
                    requested_role: ClientRole::Approver,
                    repository: None,
                },
                "attach-foreign",
            )),
        ),
    )
    .await;

    match reply.payload {
        Payload::Error(error) => assert_eq!(error.code, "protocol.session-not-found"),
        Payload::Catchup { .. } => panic!("a stranger attached to another principal's session"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    stop_server(task).await;
}

/// BRIEF rule 2 / F-19-7: a gate that answers differently for "not yours" and
/// "not there" is an enumeration oracle. Both replies must be the same shape,
/// down to the error code and the message template.
#[tokio::test]
async fn a_refusal_is_indistinguishable_from_a_missing_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let foreign_uid = our_uid(&tmp) + 1;
    let (paths, mut task, pool) = start_server(&tmp).await;
    let (foreign_session, _, _) = seed_foreign_session(&pool, foreign_uid).await;
    let never_existed = SessionId::new();

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    let mut replies = Vec::new();
    for (index, session_id) in [foreign_session, never_existed].into_iter().enumerate() {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::ReadSessionEvents {
                        session_id,
                        after_sequence: 0,
                        limit: 0,
                    },
                    &format!("oracle-{index}"),
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => replies.push((error, session_id)),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    let (existing, existing_id) = &replies[0];
    let (missing, missing_id) = &replies[1];
    assert_eq!(existing.code, missing.code);
    assert_eq!(existing.retryable, missing.retryable);
    // The only difference permitted is the id the caller itself supplied.
    assert_eq!(existing.message, format!("no session {existing_id}"));
    assert_eq!(missing.message, format!("no session {missing_id}"));

    stop_server(task).await;
}

#[tokio::test]
async fn close_session_keeps_foreign_and_missing_sessions_indistinguishable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let foreign_uid = our_uid(&tmp) + 1;
    let (paths, mut task, pool) = start_server(&tmp).await;
    let (foreign_session, _, _) = seed_foreign_session(&pool, foreign_uid).await;
    let missing_session = SessionId::new();
    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    let mut errors = Vec::new();
    for (index, session_id) in [foreign_session, missing_session].into_iter().enumerate() {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::CloseSession { session_id },
                    &format!("close-oracle-{index}"),
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => errors.push((error, session_id)),
            other => panic!("close must be refused before dispatch, got {other:?}"),
        }
    }
    assert_eq!(errors[0].0.code, errors[1].0.code);
    assert_eq!(errors[0].0.retryable, errors[1].0.retryable);
    assert_eq!(errors[0].0.message, format!("no session {}", errors[0].1));
    assert_eq!(errors[1].0.message, format!("no session {}", errors[1].1));
    let (state,): (String,) = sqlx::query_as("SELECT state FROM sessions WHERE id = ?")
        .bind(foreign_session.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "open", "outer ownership gate prevented mutation");
    stop_server(task).await;
}

/// Seed a document scoped to `session_id`, so its owner is that session's owner.
/// (A repository- or system-scoped document belongs to the daemon's uid, which
/// in-process IS this test's uid — the session scope is the only way to build a
/// document this principal genuinely does not own without a second OS user.)
async fn seed_foreign_document(pool: &SqlitePool, session_id: SessionId) -> DocumentId {
    let document_id = DocumentId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO documents (id, title, scope_json, scope_tier, scope_key, status, \
         metadata_json, crdt_snapshot, links_json, citations_json, revision, created_at, updated_at) \
         VALUES (?, 'their runbook', ?, 'session', ?, 'draft', '{}', X'', '[]', '[]', 1, ?, ?)",
    )
    .bind(document_id.to_string())
    .bind(format!(r#"{{"type":"Session","id":"{session_id}"}}"#))
    .bind(session_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .expect("seed document");
    document_id
}

/// F-19-A, the round-4 finding, reproduced and now refused. `PublishDocument`
/// checked the role and the transport and then built the publish, so a foreign
/// principal got the daemon to compile and durably park a Git write — against a
/// path *it* chose — into the owning user's approval queue. Worse, the reply
/// told it which document ids exist: a real one answered
/// `DocumentPublishRequested`, an absent one `document.not-found`.
#[tokio::test]
async fn a_stranger_cannot_publish_another_principals_document() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let foreign_uid = our_uid(&tmp) + 1;
    let (paths, mut task, pool, documents) = start_server_with_documents(&tmp).await;
    let (session_id, _, _) = seed_foreign_session(&pool, foreign_uid).await;
    let theirs = seed_foreign_document(&pool, session_id).await;
    let never_existed = DocumentId::new();

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;
    // Publishing is Controller-only; assert the most privileged role, which must
    // not help. (This also proves the ownership gate runs BEFORE the role gate:
    // a Controller is exactly who would otherwise get through.)
    let _ = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AttachSession {
                    session_id: SessionId::new(),
                    last_seen_sequence: None,
                    subscriptions: vec![],
                    requested_role: ClientRole::Controller,
                    repository: None,
                },
                "publish-role-bootstrap",
            )),
        ),
    )
    .await;

    let mut replies = Vec::new();
    for (index, document_id) in [theirs, never_existed].into_iter().enumerate() {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::PublishDocument {
                        document_id,
                        target: codypendent_protocol::document::PublishTarget::RepositoryFile {
                            path: "docs/attacker-chose-this.md".to_string(),
                        },
                    },
                    &format!("publish-foreign-{index}"),
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => replies.push((error, document_id)),
            Payload::DocumentPublishRequested { git_action, .. } => panic!(
                "THE REVIEW'S CONFUSED DEPUTY: a stranger parked a publish on another \
                 principal's document: {git_action}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // And the two refusals are the same answer: no enumeration oracle.
    let (existing, existing_id) = &replies[0];
    let (missing, missing_id) = &replies[1];
    assert_eq!(existing.code, "document.not-found");
    assert_eq!(existing.code, missing.code);
    assert_eq!(existing.retryable, missing.retryable);
    assert_eq!(existing.message, format!("no document {existing_id}"));
    assert_eq!(missing.message, format!("no document {missing_id}"));

    // And the publisher — wired, so this is not vacuous — was never reached, so
    // no plan was compiled and no approval parked in the owner's rail.
    assert!(
        documents.published.lock().expect("published").is_empty(),
        "the publish seam must never see another principal's document"
    );
    let (parked,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM approvals WHERE state = 'pending'")
            .fetch_one(&pool)
            .await
            .expect("count approvals");
    assert_eq!(parked, 1, "only the seeded approval may exist");

    stop_server(task).await;
}

/// F-19-B. `ReleaseDocumentLease` acted on the caller's `lease_id` with no
/// holder check at all, and the store's UPDATE carried no owner predicate — so
/// a peer that learned a lease id could drop another writer's single-writer
/// lock. The refusal has to be the SAME accepted no-op an unknown lease id
/// already gets, or the reply itself would enumerate live leases.
#[tokio::test]
async fn a_stranger_cannot_release_another_principals_document_lease() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let foreign_uid = our_uid(&tmp) + 1;
    let (paths, mut task, pool, documents) = start_server_with_documents(&tmp).await;
    let (session_id, _, _) = seed_foreign_session(&pool, foreign_uid).await;
    let document_id = seed_foreign_document(&pool, session_id).await;

    let lease_id = "lease-owned-by-someone-else";
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO document_leases \
         (id, document_id, block_id, holder_json, holder_key, state, acquired_at, expires_at) \
         VALUES (?, ?, 'p', '{\"type\":\"Human\",\"user\":\"them\"}', 'human:them', 'active', ?, ?)",
    )
    .bind(lease_id)
    .bind(document_id.to_string())
    .bind(&now)
    .bind((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339())
    .execute(&pool)
    .await
    .expect("seed lease");

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;
    let _ = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AttachSession {
                    session_id: SessionId::new(),
                    last_seen_sequence: None,
                    subscriptions: vec![],
                    requested_role: ClientRole::Controller,
                    repository: None,
                },
                "release-role-bootstrap",
            )),
        ),
    )
    .await;

    let mut replies = Vec::new();
    for (index, id) in [lease_id, "lease-that-never-existed"]
        .into_iter()
        .enumerate()
    {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::ReleaseDocumentLease {
                        lease_id: id.to_string(),
                    },
                    &format!("release-foreign-{index}"),
                )),
            ),
        )
        .await;
        // Normalize away the caller's own `command_id`, which it supplied.
        replies.push(match reply.payload {
            Payload::CommandAccepted {
                sequence,
                created_run,
                ..
            } => format!("accepted {sequence:?} {created_run:?}"),
            other => format!("{other:?}"),
        });
    }
    assert_eq!(
        replies[0], replies[1],
        "a foreign lease must answer exactly as an unknown one does"
    );

    // The refusal has to be real, not merely a different reply: the release seam
    // — wired here — was never called, so the lease is still held.
    assert!(
        documents.released.lock().expect("released").is_empty(),
        "the release seam must never see a lease over another principal's document"
    );
    let (state,): (String,) = sqlx::query_as("SELECT state FROM document_leases WHERE id = ?")
        .bind(lease_id)
        .fetch_one(&pool)
        .await
        .expect("read lease");
    assert_eq!(
        state, "active",
        "a stranger must not be able to break another writer's lease"
    );

    stop_server(task).await;
}

// --- ordinary same-user operation is unaffected -------------------------------

/// The other half of the contract: the owning principal must be able to do
/// everything it could before. Create a session over the socket, read its
/// events back, and resolve its own approval — all through the same gates.
#[tokio::test]
async fn the_owning_principal_is_unaffected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let uid = our_uid(&tmp);
    let (paths, mut task, pool) = start_server(&tmp).await;

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    // 1. Create a session over the wire.
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::CreateSession {
                    title: "mine".to_string(),
                    workspace: codypendent_protocol::WorkspaceId::new(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "create-own",
            )),
        ),
    )
    .await;
    assert!(
        matches!(reply.payload, Payload::CommandAccepted { .. }),
        "the owner must be able to create a session: {:?}",
        reply.payload
    );
    let session_id = reply.session_id.expect("CreateSession returns its session");

    // 2. The server recorded THIS principal as the owner — not the client id.
    let (owner_uid,): (Option<i64>,) =
        sqlx::query_as("SELECT owner_uid FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("read owner");
    assert_eq!(
        owner_uid,
        Some(i64::from(uid)),
        "a session must record the connecting principal's uid"
    );

    // 3. Attach and read its history back.
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AttachSession {
                    session_id,
                    last_seen_sequence: None,
                    subscriptions: vec![Subscription::SessionSummary],
                    requested_role: ClientRole::Controller,
                    repository: None,
                },
                "attach-own",
            )),
        ),
    )
    .await;
    assert!(
        matches!(reply.payload, Payload::Catchup { .. }),
        "the owner must be able to attach: {:?}",
        reply.payload
    );

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ReadSessionEvents {
                    session_id,
                    after_sequence: 0,
                    limit: 0,
                },
                "read-own",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::SessionEventsPage { events, .. } => {
            assert!(!events.is_empty(), "the owner sees its own SessionCreated");
        }
        other => panic!("the owner must be able to page its own history, got {other:?}"),
    }

    // 4. Resolve an approval on its own run. The identity recorded is the peer
    //    uid, not the UUID this client chose for itself.
    let (run_id, approval_id) = seed_owned_run_and_approval(&pool, session_id).await;
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ResolveApproval {
                    approval_id,
                    decision: ApprovalDecision::Approve,
                    scope: ApprovalScope::Once,
                },
                "approve-own",
            )),
        ),
    )
    .await;
    assert!(
        matches!(reply.payload, Payload::CommandAccepted { .. }),
        "the owner must be able to resolve its own approval: {:?}",
        reply.payload
    );

    let (state, resolved_by) = approval_state(&pool, approval_id).await;
    assert_eq!(state, "approved");
    assert_eq!(
        resolved_by,
        Some(format!("uid:{uid}")),
        "resolved_by must name the OS user the kernel reported, not a client-chosen UUID"
    );
    assert_ne!(
        resolved_by,
        Some(client_id.to_string()),
        "resolved_by must no longer be the client's own envelope id"
    );
    // Principal-owned closure is accepted and remains visible through the same
    // history read path after the projection becomes closed. Finish the seeded
    // run first: live runs deliberately prevent session closure.
    sqlx::query("UPDATE runs SET state = 'Completed' WHERE id = ?")
        .bind(run_id.to_string())
        .execute(&pool)
        .await
        .expect("finish seeded run before closure");
    codypendent_daemon::ledger::append_next_event(
        &pool,
        session_id,
        &Actor::System,
        &EventBody::RunCompleted {
            run_id,
            disposition: RunDisposition::Completed { summary: None },
            chronicle: ArtifactRef {
                id: ArtifactId::new(),
                media_type: "application/json".into(),
                byte_length: 2,
                sha256: "0".repeat(64),
                sensitivity: DataClassification::Internal,
            },
        },
        chrono::Utc::now(),
    )
    .await
    .expect("append completion evidence before closure");
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::CloseSession { session_id },
                "close-own",
            )),
        ),
    )
    .await;
    assert!(matches!(reply.payload, Payload::CommandAccepted { .. }));
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ReadSessionEvents {
                    session_id,
                    after_sequence: 0,
                    limit: 0,
                },
                "read-closed-own",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::SessionEventsPage { events, .. } => assert!(events
            .iter()
            .any(|event| matches!(event.body, codypendent_protocol::EventBody::SessionClosed))),
        other => panic!("closed history remains readable, got {other:?}"),
    }

    stop_server(task).await;
}

/// The connection principal is not the client id, and cannot be steered by one:
/// two connections claiming wildly different `client_id`s are the same
/// principal, because they are the same OS user.
#[tokio::test]
async fn a_client_cannot_change_its_principal_by_choosing_a_client_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let uid = our_uid(&tmp);
    let (paths, mut task, pool) = start_server(&tmp).await;

    let mut first = connect(&paths, &mut task).await;
    let first_id = ClientId::new();
    handshake(&mut first, first_id).await;
    let reply = send_recv(
        &mut first,
        &Envelope::request(
            first_id,
            Payload::Command(command(
                CommandBody::CreateSession {
                    title: "first".to_string(),
                    workspace: codypendent_protocol::WorkspaceId::new(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "create-first",
            )),
        ),
    )
    .await;
    let session_id = reply.session_id.expect("session created");

    // A *different* client id on a *different* connection — the review's
    // "stranger". Same OS user, so same principal, so it legitimately gets in.
    let mut second = connect(&paths, &mut task).await;
    let second_id = ClientId::new();
    assert_ne!(first_id, second_id);
    handshake(&mut second, second_id).await;
    let reply = send_recv(
        &mut second,
        &Envelope::request(
            second_id,
            Payload::Command(command(
                CommandBody::ReadSessionEvents {
                    session_id,
                    after_sequence: 0,
                    limit: 0,
                },
                "read-as-peer",
            )),
        ),
    )
    .await;
    assert!(
        matches!(reply.payload, Payload::SessionEventsPage { .. }),
        "a same-uid peer IS the owner and must not be locked out: {:?}",
        reply.payload
    );

    let (owner_uid,): (Option<i64>,) =
        sqlx::query_as("SELECT owner_uid FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("read owner");
    assert_eq!(owner_uid, Some(i64::from(uid)));

    stop_server(task).await;
}

/// Seed a run + pending approval on a session the caller already owns.
async fn seed_owned_run_and_approval(
    pool: &SqlitePool,
    session_id: SessionId,
) -> (RunId, ApprovalId) {
    let run_id = RunId::new();
    let approval_id = ApprovalId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
         VALUES (?, ?, 'my run', 'AwaitingApproval', 'Build', 'hosted-default', '{}')",
    )
    .bind(run_id.to_string())
    .bind(session_id.to_string())
    .execute(pool)
    .await
    .expect("seed run");
    sqlx::query(
        "INSERT INTO approvals \
         (id, run_id, action_json, risk_json, capabilities_json, state, scope, requested_at) \
         VALUES (?, ?, ?, ?, '[]', 'pending', 'once', ?)",
    )
    .bind(approval_id.to_string())
    .bind(run_id.to_string())
    .bind(r#"{"type":"RunCommand","command":"ls -la"}"#)
    .bind(r#"{"level":"Low"}"#)
    .bind(&now)
    .execute(pool)
    .await
    .expect("seed approval");
    (run_id, approval_id)
}

/// Seed a session owned by `uid`.
async fn seed_owned_session(pool: &SqlitePool, uid: u32, title: &str) -> SessionId {
    let session_id = SessionId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sessions (id, title, state, created_at, updated_at, revision, owner_uid) \
         VALUES (?, ?, 'open', ?, ?, 0, ?)",
    )
    .bind(session_id.to_string())
    .bind(title)
    .bind(&now)
    .bind(&now)
    .bind(i64::from(uid))
    .execute(pool)
    .await
    .expect("seed owned session");
    session_id
}

/// A `ListSessions` from this principal must never return another principal's
/// session. The handler names no resource, so the ownership gate passes
/// vacuously — the query itself has to carry the owner filter.
#[tokio::test]
async fn a_stranger_cannot_list_another_principals_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let uid = our_uid(&tmp);
    let foreign_uid = uid + 1;
    let (paths, mut task, pool) = start_server(&tmp).await;

    let (foreign_session, _, _) = seed_foreign_session(&pool, foreign_uid).await;
    let own_session = seed_owned_session(&pool, uid, "my work").await;

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ListSessions {
                    workspace: None,
                    limit: None,
                },
                "list-sessions",
            )),
        ),
    )
    .await;

    match reply.payload {
        Payload::SessionList { sessions, .. } => {
            let ids: Vec<SessionId> = sessions.iter().map(|s| s.session_id).collect();
            assert!(
                ids.contains(&own_session),
                "this principal's own session must be listed"
            );
            assert!(
                !ids.contains(&foreign_session),
                "another principal's session must NOT be listed"
            );
        }
        other => panic!("expected a SessionList, got {other:?}"),
    }

    stop_server(task).await;
}

/// `SearchWorkspaceFiles` walks a client-supplied absolute path. A path the
/// caller has no owned session for must be refused rather than crawled.
#[tokio::test]
async fn a_search_outside_the_callers_scope_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (paths, mut task, _pool) = start_server(&tmp).await;

    // This principal owns no session referencing any repository, so a walk of an
    // arbitrary absolute path must be refused.
    let outside = tmp.path().join("some-other-checkout");
    std::fs::create_dir_all(&outside).expect("create outside dir");

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::SearchWorkspaceFiles {
                    repository: outside.to_string_lossy().into_owned(),
                    query: "anything".to_string(),
                    limit: None,
                },
                "search-outside",
            )),
        ),
    )
    .await;

    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "workspace.repository-not-found");
        }
        Payload::FileSearchResults { matches, .. } => panic!(
            "a search outside the caller's scope returned {} matches",
            matches.len()
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }

    stop_server(task).await;
}

/// A duplicate `ForkSession` delivery (same idempotency key) must be idempotent:
/// return the SAME forked session and create exactly one fork — not a second.
#[tokio::test]
async fn a_duplicate_fork_delivery_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let uid = our_uid(&tmp);
    let (paths, mut task, pool) = start_server(&tmp).await;

    let source = seed_owned_session(&pool, uid, "source").await;

    // A run + its ordinal-1 launch checkpoint, and the RunStarted event the fork
    // cuts at.
    let run_id = RunId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
         VALUES (?, ?, 'obj', 'Completed', 'Build', 'hosted-default', '{}')",
    )
    .bind(run_id.to_string())
    .bind(source.to_string())
    .execute(&pool)
    .await
    .expect("seed run");

    sqlx::query(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         VALUES (?, 1, ?, ?, ?, NULL, NULL, 1)",
    )
    .bind(source.to_string())
    .bind(&now)
    .bind(serde_json::to_string(&codypendent_protocol::Actor::System).unwrap())
    .bind(
        serde_json::to_string(&EventBody::RunStarted {
            run_id,
            objective: "obj".to_string(),
            mode: codypendent_protocol::AgentMode::Build,
        })
        .unwrap(),
    )
    .execute(&pool)
    .await
    .expect("seed RunStarted");

    let checkpoint_id = codypendent_protocol::CheckpointId::new();
    sqlx::query(
        "INSERT INTO run_checkpoints \
         (id, run_id, ordinal, kind, commit_sha, base_commit, worktree_path, repository_path, created_at) \
         VALUES (?, ?, 1, 'commit', ?, ?, ?, ?, ?)",
    )
    .bind(checkpoint_id.to_string())
    .bind(run_id.to_string())
    .bind("c".repeat(40))
    .bind("b".repeat(40))
    .bind(tmp.path().join("wt").to_string_lossy().into_owned())
    .bind(tmp.path().join("repo").to_string_lossy().into_owned())
    .bind(&now)
    .execute(&pool)
    .await
    .expect("seed checkpoint");

    let mut stream = connect(&paths, &mut task).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    let fork_cmd = |key: &str| {
        Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ForkSession {
                    session_id: source,
                    checkpoint: checkpoint_id,
                    name: None,
                },
                key,
            )),
        )
    };

    // Two deliveries with the SAME idempotency key.
    let first = send_recv(&mut stream, &fork_cmd("fork-once")).await;
    let second = send_recv(&mut stream, &fork_cmd("fork-once")).await;

    let fork_id_1 = match first.payload {
        Payload::SessionForked { session_id, .. } => session_id,
        other => panic!("first fork should succeed, got {other:?}"),
    };
    let fork_id_2 = match second.payload {
        Payload::SessionForked { session_id, .. } => session_id,
        other => panic!("duplicate fork should replay, got {other:?}"),
    };
    assert_eq!(
        fork_id_1, fork_id_2,
        "a duplicate delivery must return the same forked session"
    );

    // Exactly one fork exists off the source — the duplicate did not fork again.
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sessions WHERE forked_from_session_id = ?")
            .bind(source.to_string())
            .fetch_one(&pool)
            .await
            .expect("count forks");
    assert_eq!(
        count, 1,
        "a duplicate delivery must not create a second fork"
    );

    stop_server(task).await;
}
