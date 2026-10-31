//! Session-server behaviour over a real Unix socket (STEP 1.11): handshake,
//! attach + catch-up (events and snapshot), multi-client fan-out, resume from a
//! sequence, and role enforcement. Drives `server::run` exactly like
//! `tests/socket.rs`, exchanging framed envelopes over a `UnixStream`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use codypendent_daemon::executor::{RunExecutor, RunLaunch};
use codypendent_daemon::{db, instance, ledger, server};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, Actor, AgentMode, AutomationBindingDraft, AutomationBindingId,
    AutomationBindingPatch, AutomationBindingQuery, AutomationBindingRequest, Catchup,
    ClientCapabilities, ClientHello, ClientId, ClientRole, Command, CommandBody, CommandId,
    Envelope, EventBody, InboxEntryId, InboxMutation, ModelId, Payload, RepositoryId, RunId,
    SessionEvent, SessionId, Subscription, TriggerSource, WorkflowId, WorkspaceId, PROTOCOL_V1,
};
use sqlx::SqlitePool;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

type ServerTask = JoinHandle<anyhow::Result<()>>;

/// Boot a daemon on a temp data dir; return its paths and the server task.
async fn start_server(tmp: &tempfile::TempDir) -> (RuntimePaths, ServerTask) {
    let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
    paths.ensure_directories().expect("create directories");
    let pool = db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db");
    let boot = instance::record_boot(&pool).await.expect("record boot");
    let task = tokio::spawn(server::run(pool, paths.clone(), boot));
    await_server_ready(&paths).await;
    (paths, task)
}

/// A [`RunExecutor`] that records every launch instead of running it, so a test
/// can assert what the server bound onto a run (repository/model/...). Leaving
/// `collaborators` at its default `None` keeps the server on its own fresh
/// fan-out, exactly as the executor-less path.
#[derive(Clone, Default)]
struct CapturingExecutor {
    launches: Arc<Mutex<Vec<RunLaunch>>>,
    /// Repository roots the server asked to warm (code-graph scan on session
    /// open) — recorded synchronously so a test can assert the scan fired.
    scans: Arc<Mutex<Vec<std::path::PathBuf>>>,
}

impl RunExecutor for CapturingExecutor {
    fn spawn_run(&self, launch: RunLaunch) {
        self.launches.lock().expect("launches lock").push(launch);
    }

    fn ensure_repository_scanned(&self, root: std::path::PathBuf) {
        self.scans.lock().expect("scans lock").push(root);
    }
}

/// Like [`start_server`], but with an injected [`RunExecutor`] so a test can
/// observe the [`RunLaunch`]es the server builds from accepted commands.
async fn start_server_with_executor(
    tmp: &tempfile::TempDir,
    executor: Arc<dyn RunExecutor>,
) -> (RuntimePaths, ServerTask) {
    let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
    paths.ensure_directories().expect("create directories");
    let pool = db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db");
    let boot = instance::record_boot(&pool).await.expect("record boot");
    let task = tokio::spawn(server::run_with_executor(
        pool,
        paths.clone(),
        boot,
        Some(executor),
    ));
    await_server_ready(&paths).await;
    (paths, task)
}

/// A second pool onto the same database file, for tests that seed the ledger
/// directly (WAL mode allows concurrent readers/writers).
async fn client_pool(paths: &RuntimePaths) -> SqlitePool {
    db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db (client)")
}

/// Make `repository_id` resolvable for automation bindings.
///
/// `automation::resolve_repository_path` looks the id up in `sessions` with
/// `COALESCE(owner_uid, ?) = ?`, so a row with a NULL `owner_uid` authorizes
/// whichever uid the socket peer happens to be — which is what a test driving
/// the daemon over a real Unix socket needs.
async fn seed_repository(pool: &SqlitePool, repository_id: RepositoryId, path: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sessions (id, title, state, created_at, updated_at, revision, repository_id, repository) \
         VALUES (?, 'repo fixture', 'open', ?, ?, 0, ?, ?)",
    )
    .bind(SessionId::new().to_string())
    .bind(&now)
    .bind(&now)
    .bind(repository_id.to_string())
    .bind(path)
    .execute(pool)
    .await
    .expect("seed repository fixture");
}

/// Seed a repository-scoped document straight into the daemon's store.
///
/// Ownership is the OUTER gate now (outcome 19): every command that names a
/// resource id is refused `not-found` before its role and transport checks are
/// reached, so a test that wants to exercise those checks has to name a resource
/// that really exists and really belongs to this principal.
async fn seed_document(pool: &SqlitePool) -> codypendent_protocol::DocumentId {
    let document_id = codypendent_protocol::DocumentId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO documents (id, title, scope_json, scope_tier, scope_key, status, \
         metadata_json, crdt_snapshot, links_json, citations_json, revision, created_at, updated_at) \
         VALUES (?, 'seeded', '{\"type\":\"Repository\"}', 'repository', NULL, 'draft', '{}', \
         X'', '[]', '[]', 1, ?, ?)",
    )
    .bind(document_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .expect("seed document");
    document_id
}

/// Seed an active edit lease on `document_id`, so a release command reaches the
/// arm's transport check instead of the ownership gate's idempotent no-op.
async fn seed_lease(
    pool: &SqlitePool,
    lease_id: &str,
    document_id: codypendent_protocol::DocumentId,
) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO document_leases \
         (id, document_id, block_id, holder_json, holder_key, state, acquired_at, expires_at) \
         VALUES (?, ?, NULL, '{\"type\":\"Human\",\"user\":\"me\"}', 'human:me', 'active', ?, ?)",
    )
    .bind(lease_id)
    .bind(document_id.to_string())
    .bind(&now)
    .bind((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339())
    .execute(pool)
    .await
    .expect("seed lease");
}

/// Seed a durable workflow run owned by this process's uid — the same principal
/// the daemon under test runs as, so its lifecycle commands reach the role and
/// transport gates instead of the ownership one.
async fn seed_owned_workflow_run(pool: &SqlitePool, id: &str, owner_uid: u32) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO workflow_runs (id, workflow_id, workflow_version, graph_signature, run_id, \
         inputs_json, state, created_at, updated_at, owner_uid) \
         VALUES (?, 'wf', 1, 'sig', NULL, '{}', 'running', ?, ?, ?)",
    )
    .bind(id)
    .bind(&now)
    .bind(&now)
    .bind(i64::from(owner_uid))
    .execute(pool)
    .await
    .expect("seed workflow run");
}

/// This process's uid, taken from a file it creates (no `libc` dependency).
fn our_uid(tmp: &tempfile::TempDir) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    let probe = tmp.path().join(".uid-probe");
    std::fs::write(&probe, b"x").expect("write probe");
    std::fs::metadata(&probe).expect("stat probe").uid()
}

async fn connect(paths: &RuntimePaths) -> UnixStream {
    for _ in 0..250 {
        match UnixStream::connect(&paths.socket_path).await {
            Ok(stream) => return stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!(
        "timed out connecting to daemon socket {}",
        paths.socket_path.display()
    );
}

/// A bound Unix socket can accept queued connections before the daemon has
/// completed its fallible startup transactions. Prove the accept loop is live
/// before a test opens a second database pool or seeds fixtures; otherwise that
/// write can race legacy-session adoption, make startup exit, and leave later
/// `connect` calls retrying a dead socket forever.
async fn await_server_ready(paths: &RuntimePaths) {
    let mut stream = connect(paths).await;
    handshake(&mut stream, ClientId::new()).await;
}

/// Read the next frame with a generous timeout so a hang fails fast.
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

async fn send_recv(stream: &mut UnixStream, request: &Envelope) -> Envelope {
    write_envelope(stream, request).await.expect("write frame");
    read_frame(stream).await
}

fn command(body: CommandBody, key: &str) -> Command {
    Command {
        command_id: CommandId::new(),
        idempotency_key: key.to_string(),
        expected_revision: None,
        body,
    }
}

/// Handshake and assert the negotiated protocol.
async fn handshake(stream: &mut UnixStream, client_id: ClientId) {
    let hello = ClientHello {
        client_name: "server-it".to_string(),
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
    match reply.payload {
        Payload::ServerHello(server_hello) => {
            assert_eq!(server_hello.selected_protocol, PROTOCOL_V1);
            assert_eq!(server_hello.heartbeat_interval_ms, 15_000);
            assert!(
                !server_hello.build_id.is_empty(),
                "ServerHello.build_id must carry the daemon's real BUILD_ID, not a placeholder"
            );
            assert_eq!(
                server_hello.build_id,
                codypendent_protocol::BUILD_ID,
                "the daemon must report its own compile-time BUILD_ID"
            );
        }
        other => panic!("expected ServerHello, got {other:?}"),
    }
}

/// Attach to a session and return the catch-up. `key` is arbitrary — attach is
/// intercepted before the idempotent write path.
async fn attach(
    stream: &mut UnixStream,
    client_id: ClientId,
    session_id: SessionId,
    last_seen: Option<u64>,
    role: ClientRole,
    subscriptions: Vec<Subscription>,
    key: &str,
) -> Catchup {
    let body = CommandBody::AttachSession {
        session_id,
        last_seen_sequence: last_seen,
        subscriptions,
        requested_role: role,
        repository: None,
    };
    let reply = send_recv(
        stream,
        &Envelope::request(client_id, Payload::Command(command(body, key))),
    )
    .await;
    match reply.payload {
        Payload::Catchup { catchup } => catchup,
        other => panic!("expected Catchup, got {other:?}"),
    }
}

/// Bind `role` to the connection via an attach targeting a throwaway session
/// id. The attach itself is REJECTED (`protocol.session-not-found`) — the
/// daemon refuses to fabricate an empty catch-up for an unknown session — but
/// the requested role still binds to the connection, which is all a role
/// bootstrap needs.
async fn bind_role(stream: &mut UnixStream, client_id: ClientId, role: ClientRole, key: &str) {
    let reply = send_recv(
        stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AttachSession {
                    session_id: SessionId::new(),
                    last_seen_sequence: None,
                    subscriptions: vec![Subscription::SessionSummary],
                    requested_role: role,
                    repository: None,
                },
                key,
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::Error(error) => assert_eq!(error.code, "protocol.session-not-found"),
        other => panic!("expected session-not-found for role bootstrap, got {other:?}"),
    }
}

/// Read frames (ignoring stray heartbeat Pings) until an `Event` arrives.
async fn read_until_event(stream: &mut UnixStream) -> SessionEvent {
    for _ in 0..6 {
        match read_frame(stream).await.payload {
            Payload::Event(event) => return event,
            Payload::Ping => continue,
            other => panic!("expected Event, got {other:?}"),
        }
    }
    panic!("no Event arrived");
}

async fn shutdown(mut stream: UnixStream, task: ServerTask) {
    write_envelope(
        &mut stream,
        &Envelope::request(ClientId::new(), Payload::Shutdown),
    )
    .await
    .expect("write shutdown");
    // Skip any frames still in flight when Shutdown is sent — a heartbeat Ping or
    // an event/presence notification the subscription queued after catchup — until
    // the ShutdownAck arrives. (The undrained frame is delivery-order dependent,
    // which made a single read here flaky under CI scheduling.)
    let mut acked = false;
    for _ in 0..16 {
        if matches!(read_frame(&mut stream).await.payload, Payload::ShutdownAck) {
            acked = true;
            break;
        }
    }
    assert!(acked, "ShutdownAck must arrive");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("server stops within 5s")
        .expect("server task must not panic")
        .expect("server stops cleanly");
}

fn note_event(sequence: u64, text: &str) -> SessionEvent {
    SessionEvent {
        sequence,
        occurred_at: chrono::Utc::now(),
        causation_id: None,
        correlation_id: None,
        actor: Actor::System,
        body: EventBody::NoteAppended {
            text: text.to_string(),
            run_id: None,
        },
    }
}

/// The single session id currently stored (used to learn a created session's id,
/// which `CommandAccepted` does not carry).
async fn only_session_id(pool: &SqlitePool) -> SessionId {
    let (id,): (String,) = sqlx::query_as("SELECT id FROM sessions LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("one session");
    id.parse().expect("valid session id")
}

#[tokio::test]
async fn handshake_returns_server_hello() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let mut stream = connect(&paths).await;
    handshake(&mut stream, ClientId::new()).await;
    shutdown(stream, task).await;
}

/// `SearchSessions` is a real read command, not merely a reserved protocol
/// shape: a handshaken desktop/IDE client can find a session created through
/// the ordinary durable command path and receives its stable deep link.
#[tokio::test]
async fn search_sessions_is_served_over_the_daemon_socket() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repository = tmp.path().join("repository");
    std::fs::create_dir(&repository).expect("create repository fixture");
    let repository = repository.canonicalize().expect("canonical repository");
    let repository_id = codypendent_knowledge::stable_repository_id(&repository);
    let (paths, task) = start_server(&tmp).await;
    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;
    let workspace_id = WorkspaceId::new();

    let created = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: workspace_id,
                    title: "Investigate parser latency".to_string(),
                    repository: Some(repository.display().to_string()),
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "library-create",
            )),
        ),
    )
    .await;
    let session_id = created
        .session_id
        .expect("create reply carries the new session id");
    let pool = client_pool(&paths).await;
    let indexed_title: (String,) = sqlx::query_as(
        "SELECT content_hash FROM session_search_sources \
         WHERE session_id = ? AND source_type = 'title' AND source_id = 'title'",
    )
    .bind(session_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("created title is indexed transactionally");
    assert_eq!(indexed_title.0.len(), 64, "title index stores a SHA-256");

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::SearchSessions {
                    query: codypendent_protocol::SessionSearchQuery {
                        query: "parser latency".to_string(),
                        filters: codypendent_protocol::SessionSearchFilters {
                            repository_ids: vec![repository_id],
                            ..Default::default()
                        },
                        limit: 10,
                        cursor: None,
                    },
                },
                "library-search",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::SessionSearchResults { page, .. } => {
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].session.session_id, session_id);
            assert_eq!(page.items[0].session.workspace_id, Some(workspace_id));
            assert_eq!(page.items[0].session.repository_id, Some(repository_id));
            assert_eq!(
                page.items[0].session.repository.as_deref(),
                Some(repository.to_string_lossy().as_ref())
            );
            assert_eq!(
                page.items[0].deep_link,
                codypendent_protocol::SessionDeepLink::Session { session_id }
            );
        }
        other => panic!("expected SessionSearchResults, got {other:?}"),
    }

    shutdown(stream, task).await;
}

async fn mutate_session(
    stream: &mut UnixStream,
    client_id: ClientId,
    session_id: SessionId,
    action: codypendent_protocol::SessionLifecycleAction,
    key: &str,
) -> Envelope {
    send_recv(
        stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::MutateSessionLifecycle { session_id, action },
                key,
            )),
        ),
    )
    .await
}

/// Task 2.1 lifecycle coverage at the protocol boundary. Each successful
/// mutation returns the authoritative projection, while retention deletion
/// leaves auditable metadata but removes the session from every read surface.
#[tokio::test]
async fn session_lifecycle_mutations_and_retention_delete_are_served_over_the_socket() {
    use codypendent_protocol::{SessionDeletionMode, SessionLifecycleAction};

    let tmp = tempfile::tempdir().expect("tempdir");
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;

    let created = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "lifecycle original".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "lifecycle-create-it",
            )),
        ),
    )
    .await;
    let session_id = created.session_id.expect("created session id");

    let cases = [
        (
            SessionLifecycleAction::Rename {
                title: "  lifecycle renamed  ".to_string(),
            },
            "lifecycle-rename-it",
            "lifecycle renamed",
            false,
            false,
        ),
        (
            SessionLifecycleAction::Pin,
            "lifecycle-pin-it",
            "lifecycle renamed",
            true,
            false,
        ),
        (
            SessionLifecycleAction::Unpin,
            "lifecycle-unpin-it",
            "lifecycle renamed",
            false,
            false,
        ),
        (
            SessionLifecycleAction::Archive,
            "lifecycle-archive-it",
            "lifecycle renamed",
            false,
            true,
        ),
        (
            SessionLifecycleAction::Restore,
            "lifecycle-restore-it",
            "lifecycle renamed",
            false,
            false,
        ),
    ];
    for (action, key, title, pinned, archived) in cases {
        let reply = mutate_session(&mut stream, client_id, session_id, action, key).await;
        match reply.payload {
            Payload::SessionLifecycleApplied { session, .. } => {
                assert_eq!(session.title, title);
                assert_eq!(session.pinned, pinned);
                assert_eq!(session.archived_at.is_some(), archived);
            }
            other => panic!("expected SessionLifecycleApplied, got {other:?}"),
        }
    }

    let delete_key = "lifecycle-delete-it";
    let deleted = mutate_session(
        &mut stream,
        client_id,
        session_id,
        SessionLifecycleAction::Delete {
            mode: SessionDeletionMode::RetentionPolicy,
        },
        delete_key,
    )
    .await;
    assert!(matches!(
        &deleted.payload,
        Payload::SessionDeleted { session_id: id, tombstoned: true, .. } if *id == session_id
    ));
    let metadata: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT tombstoned_at, deletion_mode, purge_after FROM sessions WHERE id = ?",
    )
    .bind(session_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("retention tombstone metadata");
    assert!(
        metadata.0.is_some(),
        "deletion records a tombstone timestamp"
    );
    assert_eq!(metadata.1.as_deref(), Some("retention_policy"));
    assert!(metadata.2.is_some(), "retention deletion schedules a purge");

    let search = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::SearchSessions {
                    query: codypendent_protocol::SessionSearchQuery {
                        query: "lifecycle renamed".to_string(),
                        filters: Default::default(),
                        limit: 10,
                        cursor: None,
                    },
                },
                "deleted-search-it",
            )),
        ),
    )
    .await;
    assert!(matches!(
        search.payload,
        Payload::SessionSearchResults { page, .. } if page.items.is_empty()
    ));

    let history = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ReadSessionHistory {
                    session_id,
                    cursor: None,
                    limit: 10,
                },
                "deleted-history-it",
            )),
        ),
    )
    .await;
    assert!(matches!(
        history.payload,
        Payload::CommandRejected(ref error) if error.code == "protocol.session-not-found"
    ));

    // A retry with the same key/body must reproduce the persisted receipt,
    // including the original command id, rather than constructing a new reply.
    let replay = mutate_session(
        &mut stream,
        client_id,
        session_id,
        SessionLifecycleAction::Delete {
            mode: SessionDeletionMode::RetentionPolicy,
        },
        delete_key,
    )
    .await;
    assert_eq!(
        serde_json::to_value(&replay.payload).expect("serialize replay"),
        serde_json::to_value(&deleted.payload).expect("serialize original"),
        "idempotent replay returns the exact persisted response"
    );

    shutdown(stream, task).await;
}

#[tokio::test]
async fn read_session_history_pages_are_stable_complete_and_non_overlapping() {
    use codypendent_protocol::SessionLifecycleAction;

    let tmp = tempfile::tempdir().expect("tempdir");
    let (paths, task) = start_server(&tmp).await;
    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;
    let created = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "paged history".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "history-create-it",
            )),
        ),
    )
    .await;
    let session_id = created.session_id.expect("created session id");
    for (index, action) in [
        SessionLifecycleAction::Pin,
        SessionLifecycleAction::Unpin,
        SessionLifecycleAction::Archive,
        SessionLifecycleAction::Restore,
    ]
    .into_iter()
    .enumerate()
    {
        let reply = mutate_session(
            &mut stream,
            client_id,
            session_id,
            action,
            &format!("history-mutation-{index}"),
        )
        .await;
        assert!(matches!(
            reply.payload,
            Payload::SessionLifecycleApplied { .. }
        ));
    }

    let mut cursor = None;
    let mut sequences = Vec::new();
    loop {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::ReadSessionHistory {
                        session_id,
                        cursor: cursor.clone(),
                        limit: 2,
                    },
                    &format!("history-page-{}", sequences.len()),
                )),
            ),
        )
        .await;
        let page = match reply.payload {
            Payload::SessionHistory { page, .. } => page,
            other => panic!("expected SessionHistory, got {other:?}"),
        };
        sequences.extend(page.items.iter().map(|event| event.sequence));
        match page.next_cursor {
            Some(next) => {
                assert_ne!(cursor.as_ref(), Some(&next), "cursor must advance");
                cursor = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(sequences, vec![1, 2, 3, 4, 5]);

    shutdown(stream, task).await;
}

/// Insert a session, then a run in `state`, directly on `pool` — enough to
/// exercise `DaemonStatus.active_run_count` without driving the full command
/// pipeline.
async fn seed_run_in_state(pool: &SqlitePool, state: &str) {
    let session_id = SessionId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sessions (id, title, state, created_at, updated_at, revision) \
         VALUES (?, ?, 'open', ?, ?, 0)",
    )
    .bind(session_id.to_string())
    .bind("active-run-count-it")
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .expect("insert session");

    sqlx::query(
        "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(RunId::new().to_string())
    .bind(session_id.to_string())
    .bind("diagnose")
    .bind(state)
    .bind("Build")
    .bind("hosted-default")
    .bind("{}")
    .execute(pool)
    .await
    .expect("insert run");
}

/// `DaemonStatus.active_run_count` reflects the ledger: a set of terminal
/// runs (`Completed`/`Failed`/`Cancelled`) contributes zero, and each
/// subsequently seeded non-terminal run (including waiting/paused states)
/// increments the count by one — never undercounting a live-but-waiting run.
#[tokio::test]
async fn daemon_status_reports_build_id_and_active_run_count() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let mut stream = connect(&paths).await;
    let client_id = ClientId::new();

    let status_request = || Envelope::request(client_id, Payload::DaemonStatusRequest);
    let read_status = |reply: Envelope| match reply.payload {
        Payload::DaemonStatusResponse(status) => status,
        other => panic!("expected status response, got {other:?}"),
    };

    // Freshly booted, idle daemon: build id is real, no active runs.
    let status = read_status(send_recv(&mut stream, &status_request()).await);
    assert_eq!(status.build_id, codypendent_protocol::BUILD_ID);
    assert_eq!(status.active_run_count, 0);

    // A terminal-only ledger still reports zero active runs.
    seed_run_in_state(&pool, "Completed").await;
    seed_run_in_state(&pool, "Failed").await;
    seed_run_in_state(&pool, "Cancelled").await;
    let status = read_status(send_recv(&mut stream, &status_request()).await);
    assert_eq!(status.active_run_count, 0);

    // Every non-terminal state increments the count — including
    // WaitingForApproval/WaitingForUserInput/Paused, which are alive and must
    // never be treated as safe to restart past.
    let non_terminal = [
        "Queued",
        "Preparing",
        "Running",
        "WaitingForApproval",
        "WaitingForUserInput",
        "Paused",
        "Recovering",
    ];
    for (i, state) in non_terminal.iter().enumerate() {
        seed_run_in_state(&pool, state).await;
        let status = read_status(send_recv(&mut stream, &status_request()).await);
        assert_eq!(
            status.active_run_count,
            (i + 1) as u64,
            "state {state} must count as active"
        );
    }

    shutdown(stream, task).await;
}

/// Send `ShutdownIfIdle`, expect a clean `ShutdownAck`, and confirm the server
/// stops (mirrors [`shutdown`], but exercises the idle-guarded path).
async fn shutdown_if_idle_expect_ack(mut stream: UnixStream, task: ServerTask) {
    write_envelope(
        &mut stream,
        &Envelope::request(ClientId::new(), Payload::ShutdownIfIdle),
    )
    .await
    .expect("write shutdown-if-idle");
    let mut acked = false;
    for _ in 0..16 {
        if matches!(read_frame(&mut stream).await.payload, Payload::ShutdownAck) {
            acked = true;
            break;
        }
    }
    assert!(acked, "an idle daemon must ShutdownAck a ShutdownIfIdle");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("server stops within 5s")
        .expect("server task must not panic")
        .expect("server stops cleanly");
}

/// An idle daemon honours `ShutdownIfIdle` exactly like `Shutdown`.
#[tokio::test]
async fn shutdown_if_idle_stops_an_idle_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let stream = connect(&paths).await;
    shutdown_if_idle_expect_ack(stream, task).await;
}

/// A daemon with an active run REFUSES `ShutdownIfIdle` (carrying the count it
/// observed) and keeps serving — the safety gate that lets the client leave an
/// in-flight run untouched instead of killing it.
#[tokio::test]
async fn shutdown_if_idle_refused_when_a_run_is_active() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let mut stream = connect(&paths).await;

    // A single non-terminal run makes the daemon busy.
    seed_run_in_state(&pool, "Running").await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(ClientId::new(), Payload::ShutdownIfIdle),
    )
    .await;
    match reply.payload {
        Payload::ShutdownRefused { active_run_count } => assert_eq!(active_run_count, 1),
        other => panic!("expected ShutdownRefused, got {other:?}"),
    }

    // The daemon kept running: a follow-up request still gets a response.
    let status = send_recv(
        &mut stream,
        &Envelope::request(ClientId::new(), Payload::DaemonStatusRequest),
    )
    .await;
    assert!(
        matches!(status.payload, Payload::DaemonStatusResponse(_)),
        "daemon must keep serving after refusing an idle-shutdown"
    );

    // Clean up with an unconditional shutdown.
    shutdown(stream, task).await;
}

/// Insert a `workflow_runs` row in `state` directly on `pool` — enough to make
/// the daemon non-idle via the workflow half of `active_run_count`.
async fn seed_workflow_run(pool: &SqlitePool, state: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO workflow_runs \
           (id, workflow_id, workflow_version, graph_signature, inputs_json, state, created_at, updated_at) \
         VALUES (?, 'wf', 1, 'sig', '{}', ?, ?, ?)",
    )
    .bind(format!("wfr-{}", RunId::new()))
    .bind(state)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .expect("insert workflow_run");
}

/// A non-terminal WORKFLOW run also defers an idle-guarded shutdown — the
/// count spans both `runs` and `workflow_runs`, so a running workflow is never
/// silently killed by an auto-restart.
#[tokio::test]
async fn shutdown_if_idle_refused_when_a_workflow_run_is_active() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let mut stream = connect(&paths).await;

    // No session runs, but one live workflow run.
    seed_workflow_run(&pool, "running").await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(ClientId::new(), Payload::ShutdownIfIdle),
    )
    .await;
    match reply.payload {
        Payload::ShutdownRefused { active_run_count } => assert_eq!(active_run_count, 1),
        other => panic!("expected ShutdownRefused for an active workflow, got {other:?}"),
    }

    // A terminal workflow run does NOT keep the daemon busy: mark it completed,
    // and the idle-shutdown now succeeds.
    sqlx::query("UPDATE workflow_runs SET state = 'completed'")
        .execute(&pool)
        .await
        .expect("complete the workflow run");
    shutdown_if_idle_expect_ack(stream, task).await;
}

#[tokio::test]
async fn create_attach_and_two_clients_observe_one_event() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;

    // --- Client 1: handshake, establish a Contributor role, create a session ---
    let c1 = ClientId::new();
    let mut s1 = connect(&paths).await;
    handshake(&mut s1, c1).await;
    // CreateSession needs a role; establish Contributor via a role-bootstrap
    // attach (rejected for the unknown session, but the role binds).
    bind_role(&mut s1, c1, ClientRole::Contributor, "att-role").await;

    let reply = send_recv(
        &mut s1,
        &Envelope::request(
            c1,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "diagnose the failing test".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "create-1",
            )),
        ),
    )
    .await;
    assert!(matches!(reply.payload, Payload::CommandAccepted { .. }));
    let session_id = only_session_id(&pool).await;

    // Client 1 attaches to the real session -> Catchup::Events with SessionCreated.
    let catchup = attach(
        &mut s1,
        c1,
        session_id,
        None,
        ClientRole::Contributor,
        vec![Subscription::SessionSummary],
        "att-1",
    )
    .await;
    match catchup {
        Catchup::Events {
            from,
            through,
            events,
        } => {
            assert_eq!(from, 1);
            assert_eq!(through, 1);
            assert_eq!(events.len(), 1);
            assert!(matches!(events[0].body, EventBody::SessionCreated { .. }));
        }
        other => panic!("expected Events catchup, got {other:?}"),
    }

    // --- Client 2: handshake, attach to the same session ---
    let c2 = ClientId::new();
    let mut s2 = connect(&paths).await;
    handshake(&mut s2, c2).await;
    let _ = attach(
        &mut s2,
        c2,
        session_id,
        None,
        ClientRole::Contributor,
        vec![Subscription::SessionSummary],
        "att-2",
    )
    .await;

    // Client 2 submits a follow-up; it now LAUNCHES a run (continuous-session
    // plan, Task 3), so it observes both the acknowledgement and the resulting
    // `RunStarted` event (any order over one socket).
    write_envelope(
        &mut s2,
        &Envelope::request(
            c2,
            Payload::Command(command(
                CommandBody::SubmitUserInput {
                    session_id,
                    text: "focus on the parser".to_string(),
                    mode: AgentMode::Build,
                    model: None,
                    envelope: None,
                },
                "input-1",
            )),
        ),
    )
    .await
    .expect("write submit");

    let mut got_accepted = false;
    let mut got_event = false;
    for _ in 0..8 {
        match read_frame(&mut s2).await.payload {
            Payload::CommandAccepted { .. } => got_accepted = true,
            Payload::Event(event) => match event.body {
                EventBody::RunStarted { .. } => got_event = true,
                // Presence is expected background noise (STEP 3.7): a client's own
                // attach publishes a `ClientPresenceChanged` it then observes.
                EventBody::ClientPresenceChanged { .. } => {}
                other => panic!("unexpected event on client 2: {other:?}"),
            },
            Payload::Ping => {}
            other => panic!("unexpected frame on client 2: {other:?}"),
        }
        if got_accepted && got_event {
            break;
        }
    }
    assert!(got_accepted, "submitter must receive CommandAccepted");
    assert!(got_event, "submitter (subscribed) must observe the event");

    // Client 1, the second observer of the same session, receives the same event
    // (skipping the presence events either client's attach produced).
    let observed = loop {
        let event = read_until_event(&mut s1).await;
        if !matches!(event.body, EventBody::ClientPresenceChanged { .. }) {
            break event;
        }
    };
    assert!(matches!(observed.body, EventBody::RunStarted { .. }));

    shutdown(s1, task).await;
    drop(s2);
}

/// Send a command and return the correlated acceptance's run id, tolerating
/// interleaved events/pings/presence frames and command-specific acceptance
/// payloads (`RunEditorAction` uses `EditorActionAccepted`).
async fn command_created_run(
    stream: &mut UnixStream,
    client_id: ClientId,
    body: CommandBody,
    key: &str,
) -> Option<RunId> {
    let command = command(body, key);
    let expected_command = command.command_id;
    write_envelope(
        stream,
        &Envelope::request(client_id, Payload::Command(command)),
    )
    .await
    .expect("write command");
    for _ in 0..12 {
        match read_frame(stream).await.payload {
            Payload::CommandAccepted {
                command_id,
                created_run,
                ..
            } if command_id == expected_command => return created_run,
            Payload::EditorActionAccepted { command_id, run_id }
                if command_id == expected_command =>
            {
                return Some(run_id);
            }
            Payload::CommandAccepted { .. } | Payload::EditorActionAccepted { .. } => continue,
            Payload::Event(_) | Payload::Ping => continue,
            other => panic!("expected a run acceptance, got {other:?}"),
        }
    }
    panic!("no correlated run acceptance arrived");
}

#[tokio::test]
async fn a_follow_up_launches_a_new_run_after_a_prior_run() {
    // Continuous-session plan (Task 3): `SubmitUserInput` on a session that
    // already has a (now terminal) run LAUNCHES a new, distinct run whose
    // objective is the follow-up text — no longer a bare `NoteAppended`. The
    // daemon's own server is executor-less, so a run's SEED is exercised by the
    // codypendentd/runtime tests; here the observable is that a distinct new run
    // starts and records the user's text as its objective.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let client = ClientId::new();
    let mut s = connect(&paths).await;
    handshake(&mut s, client).await;

    // Create a session, then attach as Controller (may start, cancel, submit).
    let created = send_recv(
        &mut s,
        &Envelope::request(
            client,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "conversation".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "create",
            )),
        ),
    )
    .await;
    let session_id = created.session_id.expect("session id in CommandAccepted");
    attach(
        &mut s,
        client,
        session_id,
        Some(0),
        ClientRole::Controller,
        vec![Subscription::SessionSummary],
        "attach",
    )
    .await;

    // A first run, cancelled to a terminal state — the prior run in the session.
    let first_run = command_created_run(
        &mut s,
        client,
        CommandBody::StartRun {
            session_id,
            objective: "first objective".to_string(),
            mode: AgentMode::Build,
            repository: None,
            model: None,
        },
        "start",
    )
    .await
    .expect("StartRun creates a run");
    let _ = command_created_run(
        &mut s,
        client,
        CommandBody::CancelRun { run_id: first_run },
        "cancel",
    )
    .await;

    // The follow-up must launch a NEW, distinct run — not record a bare note.
    let second_run = command_created_run(
        &mut s,
        client,
        CommandBody::SubmitUserInput {
            session_id,
            text: "the follow up".to_string(),
            mode: AgentMode::Build,
            model: None,
            envelope: None,
        },
        "input",
    )
    .await
    .expect("SubmitUserInput now launches a run");
    assert_ne!(
        second_run, first_run,
        "the follow-up starts a distinct new run"
    );

    // The session ledger records that run's `RunStarted` with the follow-up text.
    let pool = client_pool(&paths).await;
    let events = ledger::load_events(&pool, session_id)
        .await
        .expect("load events");
    assert!(
        events.iter().any(|e| matches!(
            &e.body,
            EventBody::RunStarted { run_id, objective, .. }
                if *run_id == second_run && objective == "the follow up"
        )),
        "the follow-up's RunStarted must carry the user's text as its objective"
    );

    shutdown(s, task).await;
}

#[tokio::test]
async fn a_continuation_inherits_the_session_repository_and_pinned_model() {
    // I-1/I-2: a follow-up (`SubmitUserInput`) carries no repository/model on the
    // wire, yet its launched run must inherit the SESSION's provenance from its
    // originating `StartRun` — the SAME repository (not the daemon's frozen
    // `current_dir()`) and the SAME pinned model (not `None`). A capturing
    // executor records each launch so we can assert exactly what the server bound.
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo.path().to_path_buf();
    // The session's repository must differ from the daemon's process cwd — the
    // whole point of I-1 is that `current_dir()` is NOT what a follow-up runs
    // against on a shared daemon whose cwd froze at startup.
    assert_ne!(
        repo_path,
        std::env::current_dir().unwrap(),
        "the test repository must differ from the process cwd"
    );
    let pinned = ModelId("pinned-model-x".to_string());

    let capture = Arc::new(CapturingExecutor::default());
    let launches = capture.launches.clone();

    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server_with_executor(&tmp, capture).await;

    let client = ClientId::new();
    let mut s = connect(&paths).await;
    handshake(&mut s, client).await;

    let created = send_recv(
        &mut s,
        &Envelope::request(
            client,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "conversation".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "cont-create",
            )),
        ),
    )
    .await;
    let session_id = created.session_id.expect("session id in CommandAccepted");
    attach(
        &mut s,
        client,
        session_id,
        Some(0),
        ClientRole::Controller,
        vec![Subscription::SessionSummary],
        "cont-attach",
    )
    .await;

    // The originating run: bound to an explicit repository and pinned to a model.
    let first_run = command_created_run(
        &mut s,
        client,
        CommandBody::StartRun {
            session_id,
            objective: "first objective".to_string(),
            mode: AgentMode::Build,
            repository: Some(repo_path.to_string_lossy().into_owned()),
            model: Some(pinned.clone()),
        },
        "cont-start",
    )
    .await
    .expect("StartRun creates a run");

    // The follow-up: carries neither repository nor model on the wire.
    let second_run = command_created_run(
        &mut s,
        client,
        CommandBody::SubmitUserInput {
            session_id,
            text: "the follow up".to_string(),
            mode: AgentMode::Build,
            model: None,
            envelope: None,
        },
        "cont-input",
    )
    .await
    .expect("SubmitUserInput launches a run");
    assert_ne!(second_run, first_run, "the follow-up starts a distinct run");

    // `spawn_run` is called synchronously while applying the command, before its
    // `CommandAccepted` is written, so both launches are already captured here.
    let (repository, model) = {
        let captured = launches.lock().expect("launches lock");
        let continuation = captured
            .iter()
            .find(|launch| launch.run_id == second_run)
            .expect("the continuation launch was captured");
        (continuation.repository.clone(), continuation.model.clone())
    };
    assert_eq!(
        repository, repo_path,
        "the continuation runs against the session's repository, not the daemon cwd"
    );
    assert_ne!(
        repository,
        std::env::current_dir().unwrap(),
        "the continuation must NOT fall back to the process cwd"
    );
    assert_eq!(
        model,
        Some(pinned),
        "the continuation inherits the session's pinned model, not None"
    );

    shutdown(s, task).await;
}

#[tokio::test]
async fn continuation_and_editor_action_with_invalid_provenance_fail_without_dispatch() {
    // The command transaction has already created a Queued run by the time the
    // server resolves its session repository. If that lookup fails, the run
    // must become durably Failed and no executor may receive a cwd-based launch.
    let capture = Arc::new(CapturingExecutor::default());
    let launches = capture.launches.clone();
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server_with_executor(&tmp, capture).await;

    let client = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client).await;
    let created = send_recv(
        &mut stream,
        &Envelope::request(
            client,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "invalid provenance".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "invalid-prov-create",
            )),
        ),
    )
    .await;
    let session_id = created.session_id.expect("session id");
    attach(
        &mut stream,
        client,
        session_id,
        Some(0),
        ClientRole::Controller,
        vec![Subscription::SessionSummary],
        "invalid-prov-attach",
    )
    .await;

    let pool = client_pool(&paths).await;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO commands \
         (id, idempotency_key, session_id, client_id, body, status, result_json, received_at, applied_at) \
         VALUES (?, ?, ?, ?, ?, 'applied', '{}', ?, ?)",
    )
    .bind(CommandId::new().to_string())
    .bind("invalid-prov-row")
    .bind(session_id.to_string())
    .bind(client.to_string())
    .bind("{\"type\":\"StartRun\"")
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .expect("plant malformed provenance");

    let run_id = command_created_run(
        &mut stream,
        client,
        CommandBody::SubmitUserInput {
            session_id,
            text: "continue safely".to_string(),
            mode: AgentMode::Build,
            model: None,
            envelope: None,
        },
        "invalid-prov-input",
    )
    .await
    .expect("accepted continuation has a run");

    assert!(
        launches.lock().expect("launches lock").is_empty(),
        "no run may be dispatched when repository provenance is invalid"
    );
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id = ?")
        .bind(run_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("load run state");
    assert_eq!(state, "Failed");
    let events = ledger::load_events(&pool, session_id)
        .await
        .expect("load terminal events");
    assert!(events.iter().any(|event| matches!(
        &event.body,
        EventBody::RunCompleted {
            run_id: completed,
            disposition: codypendent_protocol::RunDisposition::Failed { reason, .. },
            ..
        } if *completed == run_id
            && reason == codypendent_daemon::commands::RUN_PROVENANCE_FAILURE_REASON
    )));

    let editor_run = command_created_run(
        &mut stream,
        client,
        CommandBody::RunEditorAction {
            session_id,
            action: codypendent_protocol::EditorNativeAction::ReviewCurrentFile,
            context: codypendent_protocol::EditorActionContext {
                ide: codypendent_protocol::IdeContextUpdate::default(),
                diagnostics: None,
                repository_id: None,
            },
            model: Some(ModelId("editor-override".to_string())),
        },
        "invalid-prov-editor",
    )
    .await
    .expect("accepted editor action has a run");
    assert!(
        launches.lock().expect("launches lock").is_empty(),
        "the editor-action path must also refuse unsafe dispatch"
    );
    let editor_state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id = ?")
        .bind(editor_run.to_string())
        .fetch_one(&pool)
        .await
        .expect("load editor run state");
    assert_eq!(editor_state, "Failed");
    let events = ledger::load_events(&pool, session_id)
        .await
        .expect("load editor terminal events");
    assert!(events.iter().any(|event| matches!(
        &event.body,
        EventBody::RunCompleted {
            run_id: completed,
            disposition: codypendent_protocol::RunDisposition::Failed { reason, .. },
            ..
        } if *completed == editor_run
            && reason == codypendent_daemon::commands::RUN_PROVENANCE_FAILURE_REASON
    )));

    shutdown(stream, task).await;
}

#[tokio::test]
async fn a_mid_conversation_repin_applies_instantly() {
    // Companion to `a_continuation_inherits_the_session_repository_and_pinned_model`
    // above, which covers the `model: None` (inherit) case. This covers the
    // complementary `model: Some(_)` (re-pin) case: a follow-up `SubmitUserInput`
    // that carries its OWN model must launch its run on THAT model — not the
    // session's original pin. `handle_request` builds this via
    // `model.clone().or(provenance.model)` (the current command's model wins,
    // falling back to provenance only when `None`).
    //
    // NOTE on this test's discriminating power: reversing the combinator to
    // `provenance.model.or(model.clone())` does NOT turn this test red. By the
    // time `handle_request` reaches this arm, `CommandProcessor::apply` has
    // already committed the current `SubmitUserInput` row as `status =
    // 'applied'`; `session_run_provenance`'s newest-first scan (it now matches
    // `SubmitUserInput` rows, not just `StartRun`, since the same fix) then
    // finds THAT row first and self-referentially resolves `provenance.model`
    // to this turn's own pinned model — so `provenance.model` and
    // `model.clone()` already agree whenever the latter is `Some(_)`, and
    // `Option::or`'s argument order only ever matters when both sides are
    // `Some` and differ. This was verified empirically (flipping the
    // combinator, and separately dropping `model.clone()` fallback entirely,
    // both left this test green). The test is still kept: it pins down the
    // observable, user-facing contract (a re-pin takes effect THIS turn) that
    // was untested before, even though it cannot isolate this one combinator's
    // argument order from `session_run_provenance`'s self-referential recovery.
    let pinned_y = ModelId("pinned-model-y".to_string());
    let repin_x = ModelId("repin-model-x".to_string());
    assert_ne!(
        pinned_y, repin_x,
        "the originating pin and the mid-conversation repin must differ"
    );

    let capture = Arc::new(CapturingExecutor::default());
    let launches = capture.launches.clone();

    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server_with_executor(&tmp, capture).await;

    let client = ClientId::new();
    let mut s = connect(&paths).await;
    handshake(&mut s, client).await;

    let created = send_recv(
        &mut s,
        &Envelope::request(
            client,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "conversation".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "repin-create",
            )),
        ),
    )
    .await;
    let session_id = created.session_id.expect("session id in CommandAccepted");
    attach(
        &mut s,
        client,
        session_id,
        Some(0),
        ClientRole::Controller,
        vec![Subscription::SessionSummary],
        "repin-attach",
    )
    .await;

    // The originating run: pinned to Y.
    let first_run = command_created_run(
        &mut s,
        client,
        CommandBody::StartRun {
            session_id,
            objective: "first objective".to_string(),
            mode: AgentMode::Build,
            repository: None,
            model: Some(pinned_y),
        },
        "repin-start",
    )
    .await
    .expect("StartRun creates a run");

    // The follow-up re-pins to X mid-conversation — through the SAME real
    // request path (`handle_request` / `CommandProcessor::apply`) the adjacent
    // inherit-case test uses.
    let second_run = command_created_run(
        &mut s,
        client,
        CommandBody::SubmitUserInput {
            session_id,
            text: "switch to X".to_string(),
            mode: AgentMode::Build,
            model: Some(repin_x.clone()),
            envelope: None,
        },
        "repin-input",
    )
    .await
    .expect("SubmitUserInput launches a run");
    assert_ne!(second_run, first_run, "the follow-up starts a distinct run");

    // `spawn_run` is called synchronously while applying the command, before its
    // `CommandAccepted` is written, so the continuation's launch is already
    // captured here.
    let model = {
        let captured = launches.lock().expect("launches lock");
        let continuation = captured
            .iter()
            .find(|launch| launch.run_id == second_run)
            .expect("the continuation launch was captured");
        continuation.model.clone()
    };
    assert_eq!(
        model,
        Some(repin_x),
        "a mid-conversation re-pin must take effect on THIS turn, not one turn later"
    );

    shutdown(s, task).await;
}

#[tokio::test]
async fn resume_from_sequence_returns_exactly_missed_events() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;

    // Seed a session with five events directly on the ledger.
    let session_id = SessionId::new();
    ledger::create_session(&pool, session_id, "resume")
        .await
        .unwrap();
    for sequence in 1..=5u64 {
        ledger::append_event(
            &pool,
            session_id,
            &note_event(sequence, &format!("n{sequence}")),
        )
        .await
        .unwrap();
    }

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;

    // Reconnect from sequence 2 -> exactly events 3, 4, 5 in order.
    let catchup = attach(
        &mut stream,
        client_id,
        session_id,
        Some(2),
        ClientRole::Observer,
        vec![Subscription::SessionSummary],
        "resume-att",
    )
    .await;
    match catchup {
        Catchup::Events {
            from,
            through,
            events,
        } => {
            assert_eq!(from, 3);
            assert_eq!(through, 5);
            let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
            assert_eq!(sequences, vec![3, 4, 5]);
        }
        other => panic!("expected Events catchup, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn reconnect_attach_event_window_preserves_closed_history_and_attribution() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let session_id = SessionId::new();
    let closer = ClientId::new();
    ledger::create_session(&pool, session_id, "closed resume")
        .await
        .unwrap();
    ledger::append_event(&pool, session_id, &note_event(1, "before close"))
        .await
        .unwrap();
    let mut closed = note_event(2, "unused");
    closed.actor = Actor::Client { client_id: closer };
    closed.body = EventBody::SessionClosed;
    ledger::append_event(&pool, session_id, &closed)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET state = 'closed' WHERE id = ?")
        .bind(session_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

    let client_id = ClientId::new();
    let mut first = connect(&paths).await;
    handshake(&mut first, client_id).await;
    drop(first);
    let mut reconnected = connect(&paths).await;
    handshake(&mut reconnected, client_id).await;
    match attach(
        &mut reconnected,
        client_id,
        session_id,
        Some(0),
        ClientRole::Observer,
        vec![Subscription::SessionSummary],
        "closed-window-reconnect",
    )
    .await
    {
        Catchup::Events {
            events, through, ..
        } => {
            assert_eq!(through, 2);
            assert!(matches!(events[0].body, EventBody::NoteAppended { .. }));
            assert!(matches!(events[1].body, EventBody::SessionClosed));
            assert!(matches!(events[1].actor, Actor::Client { client_id } if client_id == closer));
        }
        other => panic!("expected closed event-window catchup, got {other:?}"),
    }
    shutdown(reconnected, task).await;
}

#[tokio::test]
async fn reconnect_attach_snapshot_then_window_preserves_closed_history_and_attribution() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let session_id = SessionId::new();
    let closer = ClientId::new();
    ledger::create_session(&pool, session_id, "large closed resume")
        .await
        .unwrap();
    for sequence in 1..=501u64 {
        ledger::append_event(&pool, session_id, &note_event(sequence, "prior"))
            .await
            .unwrap();
    }
    let mut closed = note_event(502, "unused");
    closed.actor = Actor::Client { client_id: closer };
    closed.body = EventBody::SessionClosed;
    ledger::append_event(&pool, session_id, &closed)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET state = 'closed' WHERE id = ?")
        .bind(session_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;
    match attach(
        &mut stream,
        client_id,
        session_id,
        Some(0),
        ClientRole::Observer,
        vec![Subscription::SessionSummary],
        "closed-snapshot-reconnect",
    )
    .await
    {
        Catchup::Snapshot {
            through,
            projection,
        } => {
            assert_eq!(through, 502);
            assert!(projection.closed);
            assert_eq!(projection.last_sequence, 502);
        }
        other => panic!("expected closed snapshot catchup, got {other:?}"),
    }

    match attach(
        &mut stream,
        client_id,
        session_id,
        Some(500),
        ClientRole::Observer,
        vec![Subscription::SessionSummary],
        "closed-window-after-snapshot",
    )
    .await
    {
        Catchup::Events { events, .. } => {
            assert_eq!(events.len(), 2);
            assert!(matches!(events[0].body, EventBody::NoteAppended { .. }));
            assert!(matches!(events[1].body, EventBody::SessionClosed));
            assert!(matches!(events[1].actor, Actor::Client { client_id } if client_id == closer));
        }
        other => panic!("expected history window after snapshot, got {other:?}"),
    }
    shutdown(stream, task).await;
}

#[tokio::test]
async fn attach_far_behind_returns_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;

    // 501 events with last_seen 0 -> gap 501 > 500 -> snapshot path.
    let session_id = SessionId::new();
    ledger::create_session(&pool, session_id, "big session")
        .await
        .unwrap();
    for sequence in 1..=501u64 {
        ledger::append_event(&pool, session_id, &note_event(sequence, "x"))
            .await
            .unwrap();
    }

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;

    let catchup = attach(
        &mut stream,
        client_id,
        session_id,
        Some(0),
        ClientRole::Observer,
        vec![Subscription::SessionSummary],
        "snap-att",
    )
    .await;
    match catchup {
        Catchup::Snapshot {
            through,
            projection,
        } => {
            assert_eq!(through, 501);
            assert_eq!(projection.last_sequence, 501);
            assert_eq!(projection.session_id, session_id);
        }
        other => panic!("expected Snapshot catchup, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn observer_start_run_is_role_denied() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;

    // Bind Observer, then attempt StartRun against a session this connection
    // actually owns. Ownership is now the OUTER gate (outcome 19): a command
    // naming a session the connection's principal does not own — or one that
    // does not exist — is refused `session-not-found` before the role is even
    // consulted, so a probe cannot use `role-denied` to confirm an id. Role is
    // what remains once you have proved the resource is yours, and it still
    // denies an Observer here.
    let create = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "observed".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "obs-create",
            )),
        ),
    )
    .await;
    let session_id = create
        .session_id
        .expect("CreateSession returns its session");
    bind_role(&mut stream, client_id, ClientRole::Observer, "obs-att").await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::StartRun {
                    session_id,
                    objective: "diagnose".to_string(),
                    mode: AgentMode::Build,
                    repository: None,
                    model: None,
                },
                "start-denied",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    // The other half of the new ordering: an id this principal does not own is
    // refused as missing, never as `role-denied`.
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::StartRun {
                    session_id: SessionId::new(),
                    objective: "diagnose".to_string(),
                    mode: AgentMode::Build,
                    repository: None,
                    model: None,
                },
                "start-unowned",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.session-not-found"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn observer_cannot_acquire_a_document_lease() {
    // A lease is a precursor to writing, so — like StartRun — an Observer is denied
    // before the daemon checks whether document transport is wired. Ownership is
    // the outer gate ahead of both, so the document has to exist and be this
    // principal's for the role gate to be what answers.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let document_id = seed_document(&pool).await;

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Observer,
        "obs-lease-att",
    )
    .await;

    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AcquireDocumentLease {
                    lease: codypendent_protocol::DocumentEditLease {
                        document_id,
                        block_id: Some("p".to_string()),
                    },
                    ttl_seconds: None,
                },
                "lease-denied",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    // The other half of the ordering: a document id that names nothing is
    // refused as missing, never as `role-denied` — so a probe cannot use the
    // role refusal to confirm which document ids exist.
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AcquireDocumentLease {
                    lease: codypendent_protocol::DocumentEditLease {
                        document_id: codypendent_protocol::DocumentId::new(),
                        block_id: Some("p".to_string()),
                    },
                    ttl_seconds: None,
                },
                "lease-unowned",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "document.not-found"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn lease_commands_are_rejected_when_transport_is_unwired() {
    // The daemon's own test server injects no leaser (the executor-less path), so a
    // Contributor's lease command is rejected structurally rather than crashing the
    // connection — mirroring `MutateDocument` without a mutator.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let document_id = seed_document(&pool).await;

    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Contributor,
        "contrib-lease-att",
    )
    .await;

    // Acquire (on a document this principal owns, so the ownership gate is not
    // what answers) → transport-unavailable.
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::AcquireDocumentLease {
                    lease: codypendent_protocol::DocumentEditLease {
                        document_id,
                        block_id: None,
                    },
                    ttl_seconds: Some(60),
                },
                "lease-acquire-unwired",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "document.transport-unavailable");
        }
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    // Release → same structural rejection (and the connection survives both).
    // The lease has to exist and hang off a document this principal owns: a
    // release naming no live lease of its own is the documented idempotent
    // no-op, answered by the ownership gate before transport is consulted, so
    // that "not yours" and "not there" cannot be told apart.
    seed_lease(&pool, "lease-x", document_id).await;
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ReleaseDocumentLease {
                    lease_id: "lease-x".to_string(),
                },
                "lease-release-unwired",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "document.transport-unavailable");
        }
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn publish_document_requires_controller_then_transport_unavailable() {
    // Phase 4 STEP 4.4: `PublishDocument` is intercepted at the connection
    // level like `MutateDocument`/`StartWorkflow`. Publishing is a Git write,
    // so it is gated tighter than `MutateDocument` (which only denies
    // Observer) — only a `Controller` may issue it. The daemon's own test
    // server injects no publisher, so a Controller gets past the role gate but
    // the command is rejected structurally rather than crashing the connection.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    // Ownership is the outer gate, so publish against a document that exists and
    // belongs to this principal; an unowned one is covered by `multi_user_it`.
    let document_id = seed_document(&pool).await;

    let target = codypendent_protocol::document::PublishTarget::RepositoryFile {
        path: "docs/architecture.md".to_string(),
    };

    // A Contributor is role-denied (Controller-only, unlike MutateDocument).
    let contributor = ClientId::new();
    let mut contrib_stream = connect(&paths).await;
    handshake(&mut contrib_stream, contributor).await;
    bind_role(
        &mut contrib_stream,
        contributor,
        ClientRole::Contributor,
        "contrib-publish-att",
    )
    .await;
    let reply = send_recv(
        &mut contrib_stream,
        &Envelope::request(
            contributor,
            Payload::Command(command(
                CommandBody::PublishDocument {
                    document_id,
                    target: target.clone(),
                },
                "publish-contributor",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    // A Controller gets past the role gate but the transport is unwired.
    let controller = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, controller).await;
    bind_role(
        &mut stream,
        controller,
        ClientRole::Controller,
        "controller-publish-att",
    )
    .await;
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            controller,
            Payload::Command(command(
                CommandBody::PublishDocument {
                    document_id,
                    target,
                },
                "publish-controller",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "document.transport-unavailable");
        }
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    // The Contributor's connection is simply dropped (as `start_workflow_is_
    // role_denied_then_transport_unavailable` drops its Observer connection) —
    // only one `Shutdown` request must reach the single daemon instance.
    drop(contrib_stream);
    shutdown(stream, task).await;
}

#[tokio::test]
async fn start_workflow_is_role_denied_then_transport_unavailable() {
    // Phase 5 STEP 5.2: `StartWorkflow` is intercepted at the connection level
    // like `MutateDocument`. The daemon's own test server injects no starter, so
    // the command is rejected structurally rather than crashing the connection —
    // and an Observer is role-denied before the starter is even consulted.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let manifest =
        "schema_version: 1\nid: wf\nversion: 1\nsteps:\n  - id: a\n    tool: repository.test\n";

    // An Observer is role-denied (the role is checked before the starter).
    let observer = ClientId::new();
    let mut obs_stream = connect(&paths).await;
    handshake(&mut obs_stream, observer).await;
    bind_role(
        &mut obs_stream,
        observer,
        ClientRole::Observer,
        "obs-wf-att",
    )
    .await;
    let reply = send_recv(
        &mut obs_stream,
        &Envelope::request(
            observer,
            Payload::Command(command(
                CommandBody::StartWorkflow {
                    expected_definition_digest: None,
                    manifest: manifest.to_string(),
                    workflow_id: None,
                    inputs: serde_json::Value::Null,
                    repository: None,
                },
                "wf-observer",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    // A Contributor gets past the role gate but the transport is unwired.
    let contributor = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, contributor).await;
    bind_role(
        &mut stream,
        contributor,
        ClientRole::Contributor,
        "contrib-wf-att",
    )
    .await;
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            contributor,
            Payload::Command(command(
                CommandBody::StartWorkflow {
                    expected_definition_digest: None,
                    manifest: manifest.to_string(),
                    workflow_id: None,
                    inputs: serde_json::Value::Null,
                    repository: None,
                },
                "wf-contributor",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "workflow.transport-unavailable");
        }
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn workflow_lifecycle_requires_controller_then_transport_unavailable() {
    // Phase 5 STEP 5.2 / T9: pause/resume/retry/cancel are intercepted like
    // `StartWorkflow`, but controlling a run requires the `Controller` role (matching
    // agent-run cancel/pause/resume). A Contributor is role-denied; a Controller gets
    // past the gate but the daemon's own test server injects no lifecycle seam, so the
    // command is rejected `workflow.transport-unavailable` rather than crashing the
    // connection. An Observer is denied cancel too (role checked before the seam).
    let tmp = tempfile::tempdir().unwrap();
    let uid = our_uid(&tmp);
    let (paths, task) = start_server(&tmp).await;
    // Ownership is the outer gate ahead of role and transport, so the run has to
    // exist and belong to this principal for those two to be what answer.
    let pool = client_pool(&paths).await;
    seed_owned_workflow_run(&pool, "wfrun-1", uid).await;

    // A Contributor may start a workflow but not control one → role-denied.
    let contributor = ClientId::new();
    let mut contrib_stream = connect(&paths).await;
    handshake(&mut contrib_stream, contributor).await;
    bind_role(
        &mut contrib_stream,
        contributor,
        ClientRole::Contributor,
        "contrib-life-att",
    )
    .await;
    let reply = send_recv(
        &mut contrib_stream,
        &Envelope::request(
            contributor,
            Payload::Command(command(
                CommandBody::PauseWorkflow {
                    workflow_run_id: "wfrun-1".to_string(),
                },
                "pause-contributor",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected role-denied, got {other:?}"),
    }

    // A Controller gets past the role gate; all three lifecycle commands then hit
    // the unwired transport.
    let controller = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, controller).await;
    bind_role(
        &mut stream,
        controller,
        ClientRole::Controller,
        "controller-life-att",
    )
    .await;
    for (body, key) in [
        (
            CommandBody::PauseWorkflow {
                workflow_run_id: "wfrun-1".to_string(),
            },
            "pause-controller",
        ),
        (
            CommandBody::ResumeWorkflow {
                workflow_run_id: "wfrun-1".to_string(),
            },
            "resume-controller",
        ),
        (
            CommandBody::RetryWorkflowNode {
                workflow_run_id: "wfrun-1".to_string(),
                node_id: "verify".to_string(),
            },
            "retry-controller",
        ),
        (
            // Cancel is Controller-gated through the same seam (T9).
            CommandBody::CancelWorkflow {
                workflow_run_id: "wfrun-1".to_string(),
            },
            "cancel-controller",
        ),
    ] {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(controller, Payload::Command(command(body, key))),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => {
                assert_eq!(error.code, "workflow.transport-unavailable");
            }
            other => panic!("expected transport-unavailable for {key}, got {other:?}"),
        }
    }

    // An Observer cannot cancel a run (T9): role is checked before the seam, so the
    // read-only role is denied regardless of transport.
    let observer = ClientId::new();
    let mut obs_stream = connect(&paths).await;
    handshake(&mut obs_stream, observer).await;
    bind_role(
        &mut obs_stream,
        observer,
        ClientRole::Observer,
        "obs-cancel-att",
    )
    .await;
    let reply = send_recv(
        &mut obs_stream,
        &Envelope::request(
            observer,
            Payload::Command(command(
                CommandBody::CancelWorkflow {
                    workflow_run_id: "wfrun-1".to_string(),
                },
                "cancel-observer",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected role-denied for an Observer cancel, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn propose_and_advance_promotion_are_role_denied_then_transport_unavailable() {
    // Phase 7 STEP 7.5: `ProposePromotion`/`AdvancePromotion` are intercepted at
    // the connection level like `StartWorkflow` — open to any role but
    // `Observer` (a draft may be authored by anyone, including an
    // agent/grader). The daemon's own test server injects no gateway, so an
    // authorized role's command is rejected `promotion.transport-unavailable`
    // rather than crashing the connection.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let observer = ClientId::new();
    let mut obs_stream = connect(&paths).await;
    handshake(&mut obs_stream, observer).await;
    bind_role(
        &mut obs_stream,
        observer,
        ClientRole::Observer,
        "obs-prom-att",
    )
    .await;
    let reply = send_recv(
        &mut obs_stream,
        &Envelope::request(
            observer,
            Payload::Command(command(
                CommandBody::ProposePromotion {
                    kind: "router".to_string(),
                    name: "tool-selection".to_string(),
                    version: 1,
                    requires_permission_review: false,
                },
                "prom-observer-propose",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected role-denied, got {other:?}"),
    }
    let reply = send_recv(
        &mut obs_stream,
        &Envelope::request(
            observer,
            Payload::Command(command(
                CommandBody::AdvancePromotion {
                    candidate_id: "cand-1".to_string(),
                    action: codypendent_protocol::PromotionAction::StartShadow,
                },
                "prom-observer-advance",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected role-denied, got {other:?}"),
    }

    // A Contributor gets past the role gate but the transport is unwired.
    let contributor = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, contributor).await;
    bind_role(
        &mut stream,
        contributor,
        ClientRole::Contributor,
        "contrib-prom-att",
    )
    .await;
    let reply = send_recv(
        &mut stream,
        &Envelope::request(
            contributor,
            Payload::Command(command(
                CommandBody::ProposePromotion {
                    kind: "router".to_string(),
                    name: "tool-selection".to_string(),
                    version: 1,
                    requires_permission_review: false,
                },
                "prom-contributor-propose",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "promotion.transport-unavailable");
        }
        other => panic!("expected transport-unavailable, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn approve_and_rollback_promotion_require_controller_then_transport_unavailable() {
    // The human-approval gate (ADR-010, exit criterion 2): only a `Controller`
    // may issue `ApprovePromotion`/`RollbackPromotion` — every other role
    // (Observer here, and by construction every non-Controller role) is
    // refused BEFORE the promotion seam is ever consulted, so an agent/system
    // actor can never reach it through this connection (there is no wire
    // field for a caller to supply an actor at all — see
    // `codypendent_daemon::promotion`'s module doc). A Controller gets past the
    // gate but the daemon's own test server injects no gateway, so the command
    // is rejected `promotion.transport-unavailable` rather than crashing the
    // connection.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    // Neither an Observer nor a Contributor (only Controller) may approve or
    // roll back — this is stricter than `ProposePromotion`/`AdvancePromotion`.
    for (role, key_prefix) in [
        (ClientRole::Observer, "obs"),
        (ClientRole::Contributor, "contrib"),
        (ClientRole::Approver, "approver"),
    ] {
        let client_id = ClientId::new();
        let mut stream = connect(&paths).await;
        handshake(&mut stream, client_id).await;
        bind_role(
            &mut stream,
            client_id,
            role,
            &format!("{key_prefix}-prom-att"),
        )
        .await;

        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::ApprovePromotion {
                        candidate_id: "cand-1".to_string(),
                    },
                    &format!("{key_prefix}-approve"),
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => assert_eq!(
                error.code, "protocol.role-denied",
                "role {role:?} must not be able to approve a promotion"
            ),
            other => panic!("expected role-denied for {role:?}, got {other:?}"),
        }

        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::RollbackPromotion {
                        candidate_id: "cand-1".to_string(),
                    },
                    &format!("{key_prefix}-rollback"),
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => assert_eq!(
                error.code, "protocol.role-denied",
                "role {role:?} must not be able to roll back a promotion"
            ),
            other => panic!("expected role-denied for {role:?}, got {other:?}"),
        }
    }

    // A Controller gets past the role gate; both commands then hit the
    // unwired transport (this test server injects no gateway).
    let controller = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, controller).await;
    bind_role(
        &mut stream,
        controller,
        ClientRole::Controller,
        "controller-prom-att",
    )
    .await;
    for (body, key) in [
        (
            CommandBody::ApprovePromotion {
                candidate_id: "cand-1".to_string(),
            },
            "controller-approve",
        ),
        (
            CommandBody::RollbackPromotion {
                candidate_id: "cand-1".to_string(),
            },
            "controller-rollback",
        ),
    ] {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(controller, Payload::Command(command(body, key))),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => {
                assert_eq!(error.code, "promotion.transport-unavailable");
            }
            other => panic!("expected transport-unavailable for {key}, got {other:?}"),
        }
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn attaching_a_second_client_emits_presence() {
    // STEP 3.7: presence events let each attached client see who else is here.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    let client_a = ClientId::new();
    let client_b = ClientId::new();

    // A handshakes, creates a session, and attaches (SessionSummary sees all).
    let mut a = connect(&paths).await;
    handshake(&mut a, client_a).await;
    let created = send_recv(
        &mut a,
        &Envelope::request(
            client_a,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "handoff".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "create-1",
            )),
        ),
    )
    .await;
    let session_id = created.session_id.expect("session id in CommandAccepted");
    attach(
        &mut a,
        client_a,
        session_id,
        Some(0),
        ClientRole::Controller,
        vec![Subscription::SessionSummary],
        "attach-a",
    )
    .await;

    // A receives its own arrival first.
    let own = read_until_event(&mut a).await;
    assert!(
        matches!(&own.body, EventBody::ClientPresenceChanged { client_id, present: true, .. } if *client_id == client_a),
        "expected A's own presence, got {:?}",
        own.body
    );

    // B handshakes and attaches as a Contributor (the handoff to an IDE).
    let mut b = connect(&paths).await;
    handshake(&mut b, client_b).await;
    attach(
        &mut b,
        client_b,
        session_id,
        Some(0),
        ClientRole::Contributor,
        vec![Subscription::SessionSummary],
        "attach-b",
    )
    .await;

    // A now sees B join, with B's role — the presence signal a handoff relies on.
    let joined = read_until_event(&mut a).await;
    assert!(
        matches!(
            &joined.body,
            EventBody::ClientPresenceChanged { client_id, role, present: true }
                if *client_id == client_b && *role == ClientRole::Contributor
        ),
        "expected B's presence, got {:?}",
        joined.body
    );

    drop(b);
    shutdown(a, task).await;
}

/// Opening a session that carries its repository root warms the code graph:
/// the server asks the executor to scan it (so the edges overlay populates on
/// open, not only on the first run), and the server-level guard means opening
/// a SECOND session against the same repository does not re-scan.
#[tokio::test]
async fn creating_a_session_with_a_repository_warms_the_code_graph_once() {
    let tmp = tempfile::tempdir().unwrap();
    // A real, canonicalizable repository root: `maybe_scan_repository` refuses
    // to walk anything that is not a checkout (see `plausible_repository_root`),
    // so the fixture needs an actual `.git` to be warmed at all.
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".git")).expect("create .git");
    let executor = CapturingExecutor::default();
    let (paths, task) = start_server_with_executor(&tmp, Arc::new(executor.clone())).await;

    let c1 = ClientId::new();
    let mut s1 = connect(&paths).await;
    handshake(&mut s1, c1).await;
    bind_role(&mut s1, c1, ClientRole::Contributor, "att-role").await;

    let repo_str = repo.path().to_string_lossy().to_string();
    let create = |key: &str| {
        Envelope::request(
            c1,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "warm the graph".to_string(),
                    repository: Some(repo_str.clone()),
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                key,
            )),
        )
    };

    let reply = send_recv(&mut s1, &create("create-repo-1")).await;
    assert!(matches!(reply.payload, Payload::CommandAccepted { .. }));
    // A second session against the SAME repo must NOT re-scan (server guard).
    let reply = send_recv(&mut s1, &create("create-repo-2")).await;
    assert!(matches!(reply.payload, Payload::CommandAccepted { .. }));

    let scans = executor.scans.lock().expect("scans lock").clone();
    assert_eq!(
        scans.len(),
        1,
        "the repository is scanned exactly once across two opens, got {scans:?}"
    );
    assert_eq!(
        scans[0],
        repo.path(),
        "the server warms the session's repository root"
    );

    shutdown(s1, task).await;
}

/// Paged history (`ReadSessionEvents`): the read that closes the >500-event
/// catch-up gap. A client that falls far behind gets a `Catchup::Snapshot`
/// carrying title + active runs and NO transcript; before this command there was
/// no way to fetch the missing events at all, so the conversation could not be
/// rebuilt. This walks a session's ledger a page at a time and asserts the
/// contract a pager depends on: ascending order, a `through` cursor to pass back,
/// `has_more` that tells it when to stop, and a clamped page size.
#[tokio::test]
async fn read_session_events_pages_the_ledger_forward() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;

    let c1 = ClientId::new();
    let mut s1 = connect(&paths).await;
    handshake(&mut s1, c1).await;
    bind_role(&mut s1, c1, ClientRole::Contributor, "att-role").await;
    let reply = send_recv(
        &mut s1,
        &Envelope::request(
            c1,
            Payload::Command(command(
                CommandBody::CreateSession {
                    workspace: WorkspaceId::new(),
                    title: "a long conversation".to_string(),
                    repository: None,
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                },
                "create-history",
            )),
        ),
    )
    .await;
    assert!(matches!(reply.payload, Payload::CommandAccepted { .. }));
    let session_id = only_session_id(&pool).await;

    // Append a ledger longer than one page so paging is actually exercised.
    // `SessionCreated` is sequence 1, so this session ends at 13.
    for index in 0..12u32 {
        ledger::append_next_event(
            &pool,
            session_id,
            &Actor::System,
            &EventBody::NoteAppended {
                text: format!("note {index}"),
                run_id: None,
            },
            chrono::Utc::now(),
        )
        .await
        .expect("append");
    }

    async fn page(
        stream: &mut UnixStream,
        client_id: ClientId,
        session_id: SessionId,
        after: u64,
        limit: u32,
        key: &str,
    ) -> (Vec<SessionEvent>, u64, bool) {
        let body = CommandBody::ReadSessionEvents {
            session_id,
            after_sequence: after,
            limit,
        };
        let request = Envelope::request(client_id, Payload::Command(command(body, key)));
        match send_recv(stream, &request).await.payload {
            Payload::SessionEventsPage {
                events,
                through,
                has_more,
                session_id: replied,
                ..
            } => {
                assert_eq!(replied, session_id);
                (events, through, has_more)
            }
            other => panic!("expected SessionEventsPage, got {other:?}"),
        }
    }

    // First page: five events from the start, ascending, with more behind them.
    let (events, through, has_more) = page(&mut s1, c1, session_id, 0, 5, "hist-1").await;
    assert_eq!(events.len(), 5);
    assert!(
        events.windows(2).all(|w| w[0].sequence < w[1].sequence),
        "a page must be ascending"
    );
    assert_eq!(events[0].sequence, 1);
    assert_eq!(through, 5);
    assert!(has_more);

    // The cursor is exactly what the client passes back — no overlap, no gap.
    let (events, through, has_more) = page(&mut s1, c1, session_id, through, 5, "hist-2").await;
    assert_eq!(events[0].sequence, 6);
    assert_eq!(through, 10);
    assert!(has_more);

    // Page to the end. The ledger's true length is whatever the daemon recorded
    // (session creation plus the appends above), so the loop asserts the
    // CONTRACT rather than a hardcoded count: `has_more` is the only stop signal
    // a pager needs, and the pages tile the ledger exactly once.
    let total = ledger::next_sequence(&pool, session_id)
        .await
        .expect("latest sequence")
        - 1;
    let mut walked: Vec<u64> = (1..=10).collect();
    let mut cursor = through;
    let mut more = has_more;
    let mut round = 0;
    while more {
        round += 1;
        assert!(round < 50, "paging must terminate");
        let (events, next, has_more) = page(&mut s1, c1, session_id, cursor, 5, "hist-walk").await;
        assert!(!events.is_empty(), "has_more promised another page");
        walked.extend(events.iter().map(|event| event.sequence));
        cursor = next;
        more = has_more;
    }
    assert_eq!(cursor, total, "the last page's cursor is the ledger's end");
    assert_eq!(
        walked,
        (1..=total).collect::<Vec<_>>(),
        "the pages must tile the ledger exactly once, in order"
    );

    // A drained cursor is a fixed point: an empty page keeps `through` where it
    // was rather than silently skipping the requested window.
    let (events, drained_through, has_more) =
        page(&mut s1, c1, session_id, cursor, 5, "hist-4").await;
    assert!(events.is_empty());
    assert_eq!(drained_through, cursor);
    assert!(!has_more);

    // Zero asks for the server default, and the server clamps rather than
    // refusing — a client never has to know the page ceiling.
    let (events, _, _) = page(&mut s1, c1, session_id, 0, 0, "hist-5").await;
    assert_eq!(events.len() as u64, total);
    let (events, _, _) = page(&mut s1, c1, session_id, 0, 100_000, "hist-6").await;
    assert_eq!(events.len() as u64, total);

    // An unknown session is REJECTED rather than answered with an empty page:
    // a typo'd id must never read as "this session has no history".
    let reply = send_recv(
        &mut s1,
        &Envelope::request(
            c1,
            Payload::Command(command(
                CommandBody::ReadSessionEvents {
                    session_id: SessionId::new(),
                    after_sequence: 0,
                    limit: 5,
                },
                "hist-missing",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.session-not-found"),
        other => panic!("expected a rejection, got {other:?}"),
    }

    shutdown(s1, task).await;
}

#[tokio::test]
async fn probe_model_is_open_to_every_role_and_then_transport_unavailable() {
    // `ProbeModel` is a READ every client needs: an Observer that cannot see
    // why a run will not start is exactly the readiness gap this command
    // exists to close. So it must clear the role gate for an Observer as well
    // as a Controller, and then reach the seam — which the daemon's own test
    // server does not inject, so the answer is `model.probe-unavailable`
    // rather than a torn-down connection or a role refusal.
    //
    // That distinction is the whole test: a command that is role-denied for a
    // role it should serve is invisible in unit tests (the type exists, the
    // handler exists, the feature is simply unreachable), which is the failure
    // `role_permits`'s own doc comment names.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    for (role, key) in [
        (ClientRole::Observer, "probe-observer"),
        (ClientRole::Controller, "probe-controller"),
    ] {
        let client = ClientId::new();
        let mut conn = connect(&paths).await;
        handshake(&mut conn, client).await;
        bind_role(&mut conn, client, role, &format!("{key}-att")).await;

        for (body, what) in [
            (
                CommandBody::ProbeModel {
                    model: None,
                    network: false,
                },
                "every model",
            ),
            (
                CommandBody::ProbeModel {
                    model: Some(codypendent_protocol::ModelId("worker".to_string())),
                    network: true,
                },
                "one model, networked",
            ),
        ] {
            let reply = send_recv(
                &mut conn,
                &Envelope::request(
                    client,
                    Payload::Command(command(body, &format!("{key}-{}", what.len()))),
                ),
            )
            .await;
            match reply.payload {
                Payload::CommandRejected(error) => assert_eq!(
                    error.code, "model.probe-unavailable",
                    "{role:?} probing {what} must reach the seam, not the role gate"
                ),
                other => panic!("expected probe-unavailable for {role:?} ({what}), got {other:?}"),
            }
        }
        // The connection is dropped with the loop's `conn`; the last one
        // carries the shutdown below.
    }

    let mut closer = connect(&paths).await;
    let closer_id = ClientId::new();
    handshake(&mut closer, closer_id).await;
    shutdown(closer, task).await;
}

#[tokio::test]
async fn code_graph_commands_are_role_gated_then_transport_unavailable() {
    // `codypendent graph {build,status,show}` is intercepted at the connection
    // level like the memory and blackboard commands (the graph lives outside the
    // session ledger). A BUILD clears and rewrites the repository's whole graph,
    // so it is `Controller`-only; the two READS are open to any handshaken
    // client, an Observer included. The daemon's own test server injects no
    // code-graph seam, so a command that gets past the role gate is refused
    // `graph.transport-unavailable` rather than tearing the connection down.
    //
    // The reads must NOT be role-gated: a graph you cannot look at is the same
    // silence this command family exists to end. That is the half a "reads are
    // free" claim in a doc comment cannot enforce, so it is asserted here.
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;

    // An Observer may read the graph, but never rebuild it.
    let observer = ClientId::new();
    let mut obs = connect(&paths).await;
    handshake(&mut obs, observer).await;
    bind_role(&mut obs, observer, ClientRole::Observer, "obs-graph-att").await;

    let reply = send_recv(
        &mut obs,
        &Envelope::request(
            observer,
            Payload::Command(command(
                CommandBody::BuildCodeGraph {
                    repository: "/tmp/whatever".to_string(),
                },
                "graph-build-observer",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
        other => panic!("expected role-denied for an Observer build, got {other:?}"),
    }

    for (body, key) in [
        (
            CommandBody::ReadCodeGraphStatus {
                repository: "/tmp/whatever".to_string(),
            },
            "graph-status-observer",
        ),
        (
            CommandBody::ReadCodeGraph {
                repository: "/tmp/whatever".to_string(),
                query: codypendent_protocol::CodeGraphQuery::default(),
            },
            "graph-show-observer",
        ),
    ] {
        let reply = send_recv(
            &mut obs,
            &Envelope::request(observer, Payload::Command(command(body, key))),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => {
                assert_eq!(
                    error.code, "graph.transport-unavailable",
                    "an Observer read must reach the seam, not the role gate ({key})"
                );
            }
            other => panic!("expected transport-unavailable for {key}, got {other:?}"),
        }
    }

    // A Controller gets past the role gate and reaches the unwired seam.
    let controller = ClientId::new();
    let mut ctrl = connect(&paths).await;
    handshake(&mut ctrl, controller).await;
    bind_role(
        &mut ctrl,
        controller,
        ClientRole::Controller,
        "ctrl-graph-att",
    )
    .await;
    let reply = send_recv(
        &mut ctrl,
        &Envelope::request(
            controller,
            Payload::Command(command(
                CommandBody::BuildCodeGraph {
                    repository: "/tmp/whatever".to_string(),
                },
                "graph-build-controller",
            )),
        ),
    )
    .await;
    match reply.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "graph.transport-unavailable");
        }
        other => panic!("expected transport-unavailable, got {other:?}"),
    }

    shutdown(ctrl, task).await;
}

#[tokio::test]
async fn artifact_retrieval_is_bounded_correlated_and_hash_verified() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let client_id = ClientId::new();
    let mut stream = connect(&paths).await;
    handshake(&mut stream, client_id).await;
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Controller,
        "artifact-role",
    )
    .await;
    // Larger than the public 8 MiB response cap, but still below the upload cap.
    let bytes = vec![b'x'; 8 * 1024 * 1024 + 17];
    let stored = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::PutArtifact {
                    media_type: "text/x-diff".into(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    sensitivity: codypendent_protocol::DataClassification::Internal,
                },
                "artifact-put",
            )),
        ),
    )
    .await;
    let artifact = match stored.payload {
        Payload::ArtifactStored { artifact, .. } => artifact,
        other => panic!("expected stored artifact, got {other:?}"),
    };

    for (offset, limit, expected_len, eof) in [
        (0, 3, 3, false),
        (3, 3, 3, false),
        (0, u32::MAX, 8 * 1024 * 1024, false),
        (bytes.len() as u64 - 17, u32::MAX, 17, true),
        (bytes.len() as u64, 1, 0, true),
    ] {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::ReadArtifact {
                        artifact_id: artifact.id,
                        offset,
                        limit,
                        expected_sha256: artifact.sha256.clone(),
                    },
                    &format!("artifact-read-{offset}"),
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::ArtifactChunk {
                offset: got_offset,
                bytes_base64,
                eof: got_eof,
                sha256,
                ..
            } => {
                assert_eq!(got_offset, offset);
                assert_eq!(
                    base64::engine::general_purpose::STANDARD
                        .decode(bytes_base64)
                        .unwrap(),
                    vec![b'x'; expected_len]
                );
                assert_eq!(got_eof, eof);
                assert_eq!(sha256, artifact.sha256);
            }
            other => panic!("expected artifact chunk, got {other:?}"),
        }
    }
    let mismatch = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ReadArtifact {
                    artifact_id: artifact.id,
                    offset: 0,
                    limit: 1,
                    expected_sha256: "0".repeat(64),
                },
                "artifact-bad-hash",
            )),
        ),
    )
    .await;
    assert!(
        matches!(mismatch.payload, Payload::CommandRejected(ref error) if error.code == "artifact.hash-mismatch")
    );

    for (offset, limit, key) in [
        (0, 0, "artifact-zero-limit"),
        (bytes.len() as u64 + 1, 1, "artifact-past-eof"),
    ] {
        let reply = send_recv(
            &mut stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::ReadArtifact {
                        artifact_id: artifact.id,
                        offset,
                        limit,
                        expected_sha256: artifact.sha256.clone(),
                    },
                    key,
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => {
                assert_eq!(error.code, "artifact.request-invalid");
                assert_eq!(
                    error.message,
                    "artifact range must have a positive limit and start at or before EOF"
                );
            }
            other => panic!("expected request-invalid, got {other:?}"),
        }
    }

    // Unknown and foreign-owned IDs are deliberately indistinguishable.
    async fn unavailable(
        stream: &mut UnixStream,
        client_id: ClientId,
        artifact_id: codypendent_protocol::ArtifactId,
        sha256: &str,
        key: &str,
    ) -> (String, String) {
        let reply = send_recv(
            stream,
            &Envelope::request(
                client_id,
                Payload::Command(command(
                    CommandBody::ReadArtifact {
                        artifact_id,
                        offset: 0,
                        limit: 1,
                        expected_sha256: sha256.to_owned(),
                    },
                    key,
                )),
            ),
        )
        .await;
        match reply.payload {
            Payload::CommandRejected(error) => (error.code, error.message),
            other => panic!("expected unavailable artifact, got {other:?}"),
        }
    }
    let unknown = unavailable(
        &mut stream,
        client_id,
        codypendent_protocol::ArtifactId::new(),
        &artifact.sha256,
        "artifact-unknown",
    )
    .await;
    let pool = client_pool(&paths).await;
    sqlx::query("UPDATE artifacts SET owner_uid = ? WHERE id = ?")
        .bind(i64::from(our_uid(&tmp)) + 1)
        .bind(artifact.id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let foreign = unavailable(
        &mut stream,
        client_id,
        artifact.id,
        &artifact.sha256,
        "artifact-foreign",
    )
    .await;
    assert_eq!(unknown, foreign);
    assert_eq!(
        unknown,
        (
            "artifact.not-found".into(),
            "artifact is unavailable".into()
        )
    );

    // Restore ownership, corrupt the content-addressed blob, and prove the
    // real socket path verifies bytes rather than trusting metadata.
    sqlx::query("UPDATE artifacts SET owner_uid = ? WHERE id = ?")
        .bind(i64::from(our_uid(&tmp)))
        .bind(artifact.id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let blob = paths
        .data_dir
        .join("artifacts/sha256")
        .join(&artifact.sha256[..2])
        .join(&artifact.sha256);
    tokio::fs::write(blob, b"corrupt").await.unwrap();
    let corrupt = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ReadArtifact {
                    artifact_id: artifact.id,
                    offset: 0,
                    limit: 1,
                    expected_sha256: artifact.sha256.clone(),
                },
                "artifact-corrupt",
            )),
        ),
    )
    .await;
    assert!(
        matches!(corrupt.payload, Payload::CommandRejected(ref error)
        if error.code == "artifact.hash-mismatch" && error.message == "artifact integrity verification failed")
    );

    shutdown(stream, task).await;
}

async fn seed_automation_binding(
    pool: &SqlitePool,
    name: &str,
    owner_uid: u32,
) -> AutomationBindingId {
    let id = AutomationBindingId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO automation_bindings (
            id, owner_uid, name, source_type, source_json, endpoint_id,
            cron_expression, cron_timezone, one_time_at, next_fire_at, last_fire_at,
            workflow_id, workflow_version, repository_id, repository_path,
            filters_json, invocation_json, dedup_window_seconds, concurrency,
            missed_run, missed_run_max_occurrences, retry_max_attempts,
            retry_initial_delay_seconds, retry_backoff_multiplier, retry_max_delay_seconds,
            budget_wall_time_seconds, budget_tool_calls, budget_tokens, budget_cost_micros,
            approval_mode, approval_receipt, enabled, created_at, updated_at
        ) VALUES (
            ?, ?, ?, 'manual', '{\"type\":\"manual\"}', NULL,
            NULL, NULL, NULL, NULL, NULL,
            ?, '1', ?, 'repo-path',
            '{}', '{}', 86400, 'allow',
            'skip', NULL, 0,
            30, 2, NULL,
            NULL, NULL, NULL, NULL,
            'inherit', NULL, 1, ?, ?
        )",
    )
    .bind(id.to_string())
    .bind(i64::from(owner_uid))
    .bind(name)
    // `workflow_id` and `repository_id` are parsed back as UUIDs when the row is
    // read, so the seed must write real ones rather than placeholder strings.
    .bind(WorkflowId::new().to_string())
    .bind(RepositoryId::new().to_string())
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .expect("seed automation binding");
    id
}

#[tokio::test]
async fn manage_automation_binding_crud_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let mut stream = connect(&paths).await;
    let client_id = ClientId::new();

    handshake(&mut stream, client_id).await;

    // Attach as Controller
    // Role bootstrap only: the attach targets a throwaway session id and is
    // rejected with `protocol.session-not-found`, but the requested role still
    // binds to the connection. `attach` would demand a Catchup that the daemon
    // correctly refuses to fabricate for an unknown session.
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Controller,
        "attach-ctrl",
    )
    .await;

    let repo_id = RepositoryId::new();
    let wf_id = WorkflowId::new();
    // The daemon only accepts a binding for a repository it can resolve.
    seed_repository(&client_pool(&paths).await, repo_id, "/tmp/repo-crud").await;

    let draft = AutomationBindingDraft {
        name: "nightly-ci".to_string(),
        source: TriggerSource::Manual,
        workflow_id: wf_id,
        workflow_version: "1".to_string(),
        repository_id: repo_id,
        filters: Default::default(),
        invocation: Default::default(),
        enabled: true,
    };

    // 1. Create binding
    let create_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Create {
                        binding: draft.clone(),
                    },
                },
                "create-auto-1",
            )),
        ),
    )
    .await;

    let binding = match create_reply.payload {
        Payload::AutomationBindingResult { binding, .. } => {
            assert_eq!(binding.definition.name, "nightly-ci");
            assert_eq!(binding.definition.workflow_id, wf_id);
            assert!(binding.definition.enabled);
            binding
        }
        other => panic!("expected AutomationBindingResult, got {other:?}"),
    };

    // 2. Get binding
    let get_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Get { id: binding.id },
                },
                "get-auto-1",
            )),
        ),
    )
    .await;

    match get_reply.payload {
        Payload::AutomationBindingResult {
            binding: fetched, ..
        } => {
            assert_eq!(fetched.id, binding.id);
            assert_eq!(fetched.definition.name, "nightly-ci");
        }
        other => panic!("expected AutomationBindingResult, got {other:?}"),
    }

    // 3. List bindings
    let list_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::List {
                        query: AutomationBindingQuery {
                            repository_id: Some(repo_id),
                            ..Default::default()
                        },
                    },
                },
                "list-auto-1",
            )),
        ),
    )
    .await;

    match list_reply.payload {
        Payload::AutomationBindingPage { page, .. } => {
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].id, binding.id);
        }
        other => panic!("expected AutomationBindingPage, got {other:?}"),
    }

    // 4. Update binding
    let update_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Update {
                        id: binding.id,
                        patch: AutomationBindingPatch {
                            enabled: Some(false),
                            ..Default::default()
                        },
                    },
                },
                "update-auto-1",
            )),
        ),
    )
    .await;

    match update_reply.payload {
        Payload::AutomationBindingResult {
            binding: updated, ..
        } => {
            assert_eq!(updated.id, binding.id);
            assert!(!updated.definition.enabled);
        }
        other => panic!("expected AutomationBindingResult, got {other:?}"),
    }

    // 5. Delete binding
    let delete_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Delete { id: binding.id },
                },
                "delete-auto-1",
            )),
        ),
    )
    .await;

    match delete_reply.payload {
        Payload::AutomationBindingDeleted { binding_id, .. } => {
            assert_eq!(binding_id, binding.id);
        }
        other => panic!("expected AutomationBindingDeleted, got {other:?}"),
    }

    // 6. Get deleted binding -> fails with binding-not-found
    let get_deleted = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Get { id: binding.id },
                },
                "get-deleted-auto-1",
            )),
        ),
    )
    .await;

    match get_deleted.payload {
        Payload::CommandRejected(error) => {
            assert_eq!(error.code, "automation.binding-not-found");
            assert_eq!(error.message, "automation binding is unavailable");
        }
        other => panic!("expected CommandRejected, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn manage_automation_binding_role_floors() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let uid = our_uid(&tmp);

    let seeded_id = seed_automation_binding(&pool, "observer-test", uid).await;

    let mut stream = connect(&paths).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    // Attach as Observer
    // Role bootstrap only: the attach targets a throwaway session id and is
    // rejected with `protocol.session-not-found`, but the requested role still
    // binds to the connection. `attach` would demand a Catchup that the daemon
    // correctly refuses to fabricate for an unknown session.
    bind_role(&mut stream, client_id, ClientRole::Observer, "attach-obs").await;

    // Observer CAN Get
    let get_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Get { id: seeded_id },
                },
                "obs-get",
            )),
        ),
    )
    .await;
    assert!(matches!(
        get_reply.payload,
        Payload::AutomationBindingResult { .. }
    ));

    // Observer CAN List
    let list_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::List {
                        query: Default::default(),
                    },
                },
                "obs-list",
            )),
        ),
    )
    .await;
    assert!(matches!(
        list_reply.payload,
        Payload::AutomationBindingPage { .. }
    ));

    // Observer CANNOT Create
    let create_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Create {
                        binding: AutomationBindingDraft {
                            name: "obs-create".to_string(),
                            source: TriggerSource::Manual,
                            workflow_id: WorkflowId::new(),
                            workflow_version: "1".to_string(),
                            repository_id: RepositoryId::new(),
                            filters: Default::default(),
                            invocation: Default::default(),
                            enabled: true,
                        },
                    },
                },
                "obs-create",
            )),
        ),
    )
    .await;
    match create_reply.payload {
        Payload::CommandRejected(err) => assert_eq!(err.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected role-denied, got {other:?}"),
    }

    // Observer CANNOT Update
    let update_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Update {
                        id: seeded_id,
                        patch: AutomationBindingPatch {
                            enabled: Some(false),
                            ..Default::default()
                        },
                    },
                },
                "obs-update",
            )),
        ),
    )
    .await;
    match update_reply.payload {
        Payload::CommandRejected(err) => assert_eq!(err.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected role-denied, got {other:?}"),
    }

    // Observer CANNOT Delete
    let delete_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Delete { id: seeded_id },
                },
                "obs-delete",
            )),
        ),
    )
    .await;
    match delete_reply.payload {
        Payload::CommandRejected(err) => assert_eq!(err.code, "protocol.role-denied"),
        other => panic!("expected CommandRejected role-denied, got {other:?}"),
    }

    shutdown(stream, task).await;
}

#[tokio::test]
async fn manage_automation_binding_foreign_and_absent_indistinguishable() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let foreign_uid = our_uid(&tmp) + 1;

    let foreign_id = seed_automation_binding(&pool, "foreign-binding", foreign_uid).await;
    let missing_id = AutomationBindingId::new();

    let mut stream = connect(&paths).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    // Role bootstrap only: the attach targets a throwaway session id and is
    // rejected with `protocol.session-not-found`, but the requested role still
    // binds to the connection. `attach` would demand a Catchup that the daemon
    // correctly refuses to fabricate for an unknown session.
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Controller,
        "attach-ctrl-foreign",
    )
    .await;

    let missing_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Get { id: missing_id },
                },
                "get-missing",
            )),
        ),
    )
    .await;

    let missing_err = match missing_reply.payload {
        Payload::CommandRejected(err) => (err.code, err.message),
        other => panic!("expected CommandRejected, got {other:?}"),
    };

    let foreign_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Get { id: foreign_id },
                },
                "get-foreign",
            )),
        ),
    )
    .await;

    let foreign_err = match foreign_reply.payload {
        Payload::CommandRejected(err) => (err.code, err.message),
        other => panic!("expected CommandRejected, got {other:?}"),
    };

    assert_eq!(missing_err, foreign_err);
    assert_eq!(
        missing_err,
        (
            "automation.binding-not-found".into(),
            "automation binding is unavailable".into()
        )
    );

    shutdown(stream, task).await;
}

#[tokio::test]
async fn manage_automation_binding_owner_is_server_derived() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let mut stream = connect(&paths).await;
    let client_id = ClientId::new();

    handshake(&mut stream, client_id).await;

    // Role bootstrap only: the attach targets a throwaway session id and is
    // rejected with `protocol.session-not-found`, but the requested role still
    // binds to the connection. `attach` would demand a Catchup that the daemon
    // correctly refuses to fabricate for an unknown session.
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Controller,
        "attach-ctrl-owner",
    )
    .await;

    let owner_repo_id = RepositoryId::new();
    seed_repository(&client_pool(&paths).await, owner_repo_id, "/tmp/repo-owner").await;

    let draft = AutomationBindingDraft {
        name: "owner-test-binding".to_string(),
        source: TriggerSource::Manual,
        workflow_id: WorkflowId::new(),
        workflow_version: "1".to_string(),
        repository_id: owner_repo_id,
        filters: Default::default(),
        invocation: Default::default(),
        enabled: true,
    };

    let create_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::ManageAutomationBinding {
                    request: AutomationBindingRequest::Create { binding: draft },
                },
                "create-owner-test",
            )),
        ),
    )
    .await;

    let binding = match create_reply.payload {
        Payload::AutomationBindingResult { binding, .. } => binding,
        other => panic!("expected AutomationBindingResult, got {other:?}"),
    };

    let pool = client_pool(&paths).await;
    let stored_owner: i64 =
        sqlx::query_scalar("SELECT owner_uid FROM automation_bindings WHERE id = ?")
            .bind(binding.id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(stored_owner as u32, our_uid(&tmp));

    shutdown(stream, task).await;
}

async fn seed_inbox_entry(pool: &SqlitePool, foreign_uid: u32) -> InboxEntryId {
    let id = InboxEntryId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO inbox_entries (
            id, owner_uid, repository_id, kind, state, title, summary,
            source_identity_json, dedup_key, deep_link_json,
            created_at, updated_at
        ) VALUES (?, ?, ?, 'ApprovalRequest', 'Unread', 'Title', 'Summary', '{\"type\":\"unknown\"}', ?, '{\"type\":\"unknown\"}', ?, ?)",
    )
    .bind(id.to_string())
    .bind(i64::from(foreign_uid))
    .bind(RepositoryId::new().to_string())
    .bind(format!("dedup-{}", id))
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .expect("seed foreign inbox entry");
    id
}

#[tokio::test]
async fn unauthorized_and_absent_inbox_entries_are_indistinguishable() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, task) = start_server(&tmp).await;
    let pool = client_pool(&paths).await;
    let foreign_uid = our_uid(&tmp) + 1;

    let foreign_id = seed_inbox_entry(&pool, foreign_uid).await;
    let missing_id = InboxEntryId::new();

    let mut stream = connect(&paths).await;
    let client_id = ClientId::new();
    handshake(&mut stream, client_id).await;

    // Role bootstrap only: the attach targets a throwaway session id and is
    // rejected with `protocol.session-not-found`, but the requested role still
    // binds to the connection. `attach` would demand a Catchup that the daemon
    // correctly refuses to fabricate for an unknown session.
    bind_role(
        &mut stream,
        client_id,
        ClientRole::Controller,
        "attach-ctrl-foreign",
    )
    .await;

    let missing_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::MutateInbox {
                    mutation: InboxMutation::Acknowledge {
                        entry_id: missing_id,
                    },
                },
                "mutate-missing",
            )),
        ),
    )
    .await;

    let missing_err = match missing_reply.payload {
        Payload::CommandRejected(err) => (err.code, err.message),
        other => panic!("expected CommandRejected, got {other:?}"),
    };

    let foreign_reply = send_recv(
        &mut stream,
        &Envelope::request(
            client_id,
            Payload::Command(command(
                CommandBody::MutateInbox {
                    mutation: InboxMutation::Acknowledge {
                        entry_id: foreign_id,
                    },
                },
                "mutate-foreign",
            )),
        ),
    )
    .await;

    let foreign_err = match foreign_reply.payload {
        Payload::CommandRejected(err) => (err.code, err.message),
        other => panic!("expected CommandRejected, got {other:?}"),
    };

    assert_eq!(missing_err, foreign_err);
    assert_eq!(
        missing_err,
        (
            "inbox.not-found".into(),
            "inbox entry is unavailable".into()
        )
    );

    shutdown(stream, task).await;
}
