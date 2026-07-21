//! The Phase-4 collaboration proof (STEP 4.3 client transport): two clients over
//! the **real** `codypendentd` socket exchanging live document edits.
//!
//! This exercises the whole vertical end to end — the assembly binary's wired
//! `DocumentMutator`/`DocumentLeaser` seams, the daemon's connection-level
//! interception of `MutateDocument`/`AcquireDocumentLease`, the per-document
//! `DocumentHub` fan-out, and the **client-side [`DocumentReplica`]** that
//! consumes the `DocumentSync` stream — against the actual daemon process, not a
//! mock. It lives in the crate that builds the `codypendentd` binary so
//! `CARGO_BIN_EXE_codypendentd` is defined (like `recovery_it.rs`).
//!
//! Two scenarios, one per collaboration mode:
//! 1. **Edit mode** (a `System`-scope doc): A leases a block and edits it; B
//!    (subscribed) converges to identical rendered content; B's conflicting lease
//!    attempt is refused `document.range-leased` while A holds it; A releases; B
//!    edits; both converge again.
//! 2. **Suggest mode** (an `Organization`-scope doc — suggest-by-default): A's
//!    edit lands as a *suggestion* (content unchanged both sides); accepting it
//!    applies exactly the annotated range, and both replicas converge to the
//!    byte-exact final content.

use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::Duration;

use codypendent_daemon::db;
use codypendent_knowledge::{
    render_document, BlockContent, CollaborationMode, DocumentAuthor, DocumentBlock,
    DocumentMetadata, DocumentReplica, DocumentStore, NewDocument, Scope, SuggestionStore,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, ClientCapabilities, ClientHello, ClientId, ClientRole, Command,
    CommandBody, CommandId, DocumentEditLease, DocumentId, DocumentMutation, DocumentSync,
    Envelope, OrganizationId, Payload, Subscription, UserId, PROTOCOL_V1,
};
use sqlx::SqlitePool;
use tokio::net::UnixStream;

/// Owns the spawned daemon process; kills it on drop so a panicking test never
/// leaks a child holding the socket.
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the real `codypendentd` binary against `data_dir` (quiet logs, discarded
/// output), exactly as `recovery_it.rs` does.
fn spawn_daemon(data_dir: &Path) -> Daemon {
    let child = StdCommand::new(env!("CARGO_BIN_EXE_codypendentd"))
        .env("CODYPENDENT_DATA_DIR", data_dir)
        .env_remove("CODYPENDENT_SOCKET")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codypendentd");
    Daemon { child }
}

async fn wait_for_socket(paths: &RuntimePaths) -> UnixStream {
    for _ in 0..200 {
        if let Ok(stream) = UnixStream::connect(&paths.socket_path).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "daemon socket never came up at {}",
        paths.socket_path.display()
    );
}

async fn open_pool(paths: &RuntimePaths) -> SqlitePool {
    db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db")
}

fn seed_author() -> DocumentAuthor {
    DocumentAuthor::Human {
        user: UserId("seed".to_string()),
    }
}

/// Create a single-paragraph document at `scope` before the daemon starts, so the
/// running daemon opens a DB that already holds it. Returns its id.
async fn seed_document(paths: &RuntimePaths, title: &str, scope: Scope, text: &str) -> DocumentId {
    paths.ensure_directories().expect("create directories");
    let pool = open_pool(paths).await;
    let id = DocumentStore::new()
        .create(
            &pool,
            NewDocument {
                title: title.to_string(),
                scope,
                metadata: DocumentMetadata::default(),
                blocks: vec![DocumentBlock::with_id(
                    "p",
                    BlockContent::Paragraph {
                        text: text.to_string(),
                    },
                )],
            },
            &seed_author(),
        )
        .await
        .expect("seed document")
        .id;
    pool.close().await;
    id
}

/// Read the current CRDT snapshot of `document_id` over the document read path and
/// seed a client replica from it (the baseline a subscriber starts from).
async fn seed_replica(pool: &SqlitePool, document_id: DocumentId) -> DocumentReplica {
    let doc = DocumentStore::new()
        .load(pool, document_id)
        .await
        .expect("load")
        .expect("document exists");
    DocumentReplica::from_snapshot(&doc.crdt.snapshot().expect("snapshot"), doc.revision)
        .expect("seed replica")
}

async fn read_frame(stream: &mut UnixStream) -> Envelope {
    tokio::time::timeout(Duration::from_secs(5), read_envelope(stream))
        .await
        .expect("read timed out")
        .expect("read frame")
        .expect("server must reply")
}

fn command(body: CommandBody, key: &str) -> Command {
    Command {
        command_id: CommandId::new(),
        idempotency_key: key.to_string(),
        expected_revision: None,
        body,
    }
}

async fn send(stream: &mut UnixStream, client: ClientId, body: CommandBody, key: &str) {
    write_envelope(
        stream,
        &Envelope::request(client, Payload::Command(command(body, key))),
    )
    .await
    .expect("write command");
}

/// A handshaken *local* connection binds the `Controller` role by default (see
/// `recovery_it.rs`), so no explicit role bootstrap is needed to create, lease,
/// mutate, or resolve.
async fn handshake(stream: &mut UnixStream, client: ClientId) {
    let hello = ClientHello {
        client_name: "docs-sync-it".to_string(),
        client_version: "0".to_string(),
        supported_protocols: vec![PROTOCOL_V1],
        capabilities: ClientCapabilities::default(),
        resume_token: None,
    };
    write_envelope(
        stream,
        &Envelope::request(client, Payload::ClientHello(hello)),
    )
    .await
    .expect("write hello");
    assert!(matches!(
        read_frame(stream).await.payload,
        Payload::ServerHello(_)
    ));
}

/// Attach `stream` to `session` subscribed to `document_id`'s live sync stream. A
/// real-session attach is what makes the daemon spawn this connection's document
/// forwarder; the reply is a `Catchup` we discard.
async fn attach_document(
    stream: &mut UnixStream,
    client: ClientId,
    session: codypendent_protocol::SessionId,
    document_id: DocumentId,
    key: &str,
) {
    send(
        stream,
        client,
        CommandBody::AttachSession {
            session_id: session,
            last_seen_sequence: None,
            subscriptions: vec![Subscription::Document { document_id }],
            requested_role: ClientRole::Controller,
        },
        key,
    )
    .await;
    // The attach reply is a Catchup (skip stray heartbeats before it).
    loop {
        match read_frame(stream).await.payload {
            Payload::Catchup { .. } => break,
            Payload::Ping => continue,
            other => panic!("expected Catchup on attach, got {other:?}"),
        }
    }
}

/// Read frames until the next `DocumentSync`, skipping heartbeats and this
/// client's own command acknowledgements (which may interleave with its sync).
async fn recv_document_sync(stream: &mut UnixStream) -> DocumentSync {
    for _ in 0..16 {
        match read_frame(stream).await.payload {
            Payload::DocumentSync(sync) => return sync,
            Payload::Ping | Payload::CommandAccepted { .. } | Payload::Event(_) => continue,
            other => panic!("expected DocumentSync, got {other:?}"),
        }
    }
    panic!("no DocumentSync arrived");
}

/// Read frames until the reply to a lease acquire: either the grant (with its
/// minted lease id) or the structured rejection.
async fn recv_lease_reply(stream: &mut UnixStream) -> Result<String, String> {
    for _ in 0..16 {
        match read_frame(stream).await.payload {
            Payload::DocumentLeaseGranted { grant, .. } => return Ok(grant.lease_id),
            Payload::CommandRejected(error) => return Err(error.code),
            Payload::Ping | Payload::Event(_) => continue,
            other => panic!("expected a lease reply, got {other:?}"),
        }
    }
    panic!("no lease reply arrived");
}

async fn recv_command_accepted(stream: &mut UnixStream) {
    for _ in 0..16 {
        match read_frame(stream).await.payload {
            Payload::CommandAccepted { .. } => return,
            Payload::Ping | Payload::Event(_) => continue,
            other => panic!("expected CommandAccepted, got {other:?}"),
        }
    }
    panic!("no CommandAccepted arrived");
}

/// Create a session over `stream` and return its id (rides the reply envelope).
async fn create_session(
    stream: &mut UnixStream,
    client: ClientId,
) -> codypendent_protocol::SessionId {
    send(
        stream,
        client,
        CommandBody::CreateSession {
            workspace: codypendent_protocol::WorkspaceId::new(),
            title: "docs".to_string(),
        },
        "create",
    )
    .await;
    loop {
        let env = read_frame(stream).await;
        match env.payload {
            Payload::CommandAccepted { .. } => {
                return env.session_id.expect("created session id on envelope")
            }
            Payload::Ping => continue,
            other => panic!("expected CommandAccepted for CreateSession, got {other:?}"),
        }
    }
}

fn edit_text(block: &str, position: u32, delete_len: u32, insert: &str) -> DocumentMutation {
    DocumentMutation::EditText {
        block_id: block.to_string(),
        position,
        delete_len,
        insert: insert.to_string(),
    }
}

fn block_lease(document_id: DocumentId, block: &str) -> DocumentEditLease {
    DocumentEditLease {
        document_id,
        block_id: Some(block.to_string()),
    }
}

/// Poll the store for the document's first pending suggestion id (committed before
/// its sync is published, but polled to absorb any read-after-write latency).
async fn wait_for_suggestion(pool: &SqlitePool, document_id: DocumentId) -> String {
    for _ in 0..40 {
        let pending = SuggestionStore::new()
            .pending(pool, document_id)
            .await
            .expect("read pending suggestions");
        if let Some(first) = pending.first() {
            return first.id.clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no pending suggestion appeared");
}

#[tokio::test]
async fn two_clients_converge_over_the_socket_with_lease_exclusion() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir: PathBuf = tmp.path().to_path_buf();
    let paths = RuntimePaths::from_data_dir(data_dir.clone());

    // A System-scope doc defaults to Edit mode (direct edits).
    let doc = seed_document(&paths, "Edit Doc", Scope::System, "hello").await;
    assert_eq!(
        CollaborationMode::default_for_scope(&Scope::System),
        CollaborationMode::Edit
    );

    let _daemon = spawn_daemon(&data_dir);
    let read_pool = open_pool(&paths).await;

    // Two clients, each on its own connection (distinct ClientId ⇒ distinct
    // lease-writer identity), both subscribed to the document.
    let ca = ClientId::new();
    let cb = ClientId::new();
    let mut a = wait_for_socket(&paths).await;
    let mut b = UnixStream::connect(&paths.socket_path)
        .await
        .expect("connect b");
    handshake(&mut a, ca).await;
    handshake(&mut b, cb).await;
    let session = create_session(&mut a, ca).await;
    attach_document(&mut a, ca, session, doc, "att-a").await;
    attach_document(&mut b, cb, session, doc, "att-b").await;

    // Each seeds its replica from the current document state (the read path).
    let mut replica_a = seed_replica(&read_pool, doc).await;
    let mut replica_b = seed_replica(&read_pool, doc).await;
    assert_eq!(
        replica_a.render("Edit Doc").unwrap(),
        replica_b.render("Edit Doc").unwrap()
    );

    // 1. A leases block "p" and inserts " world" after "hello".
    send(
        &mut a,
        ca,
        CommandBody::AcquireDocumentLease {
            lease: block_lease(doc, "p"),
            ttl_seconds: None,
        },
        "lease-a",
    )
    .await;
    let lease_a = recv_lease_reply(&mut a)
        .await
        .expect("A acquires the block lease");

    send(
        &mut a,
        ca,
        CommandBody::MutateDocument {
            document_id: doc,
            mutation: edit_text("p", 5, 0, " world"),
        },
        "edit-a",
    )
    .await;

    // Both clients receive the authoritative sync and converge.
    replica_a.merge(&recv_document_sync(&mut a).await).unwrap();
    replica_b.merge(&recv_document_sync(&mut b).await).unwrap();
    assert_eq!(blocks_text(&replica_a), "hello world");
    assert_eq!(
        replica_a.render("Edit Doc").unwrap(),
        replica_b.render("Edit Doc").unwrap(),
        "B converges to identical rendered content"
    );

    // 2. While A holds the lease, B's attempt on the same block is refused.
    send(
        &mut b,
        cb,
        CommandBody::AcquireDocumentLease {
            lease: block_lease(doc, "p"),
            ttl_seconds: None,
        },
        "lease-b-refused",
    )
    .await;
    assert_eq!(
        recv_lease_reply(&mut b)
            .await
            .expect_err("B is refused while A holds it"),
        "document.range-leased"
    );

    // 3. A releases; now B can lease and edit, and both converge again.
    send(
        &mut a,
        ca,
        CommandBody::ReleaseDocumentLease { lease_id: lease_a },
        "release-a",
    )
    .await;
    recv_command_accepted(&mut a).await;

    send(
        &mut b,
        cb,
        CommandBody::AcquireDocumentLease {
            lease: block_lease(doc, "p"),
            ttl_seconds: None,
        },
        "lease-b",
    )
    .await;
    let _lease_b = recv_lease_reply(&mut b)
        .await
        .expect("B acquires after A released");

    send(
        &mut b,
        cb,
        CommandBody::MutateDocument {
            document_id: doc,
            mutation: edit_text("p", 11, 0, "!"),
        },
        "edit-b",
    )
    .await;
    replica_b.merge(&recv_document_sync(&mut b).await).unwrap();
    replica_a.merge(&recv_document_sync(&mut a).await).unwrap();
    assert_eq!(blocks_text(&replica_b), "hello world!");
    assert_eq!(
        replica_a.render("Edit Doc").unwrap(),
        replica_b.render("Edit Doc").unwrap(),
        "both converge after B's edit"
    );

    read_pool.close().await;
}

#[tokio::test]
async fn suggest_mode_annotate_then_accept_is_byte_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir: PathBuf = tmp.path().to_path_buf();
    let paths = RuntimePaths::from_data_dir(data_dir.clone());

    // An Organization-scope doc defaults to Suggest mode (suggest-by-default).
    let scope = Scope::Organization(OrganizationId::new());
    assert_eq!(
        CollaborationMode::default_for_scope(&scope),
        CollaborationMode::Suggest
    );
    let doc = seed_document(&paths, "Org Doc", scope, "draft").await;

    let _daemon = spawn_daemon(&data_dir);
    let read_pool = open_pool(&paths).await;

    let ca = ClientId::new();
    let cb = ClientId::new();
    let mut a = wait_for_socket(&paths).await;
    let mut b = UnixStream::connect(&paths.socket_path)
        .await
        .expect("connect b");
    handshake(&mut a, ca).await;
    handshake(&mut b, cb).await;
    let session = create_session(&mut a, ca).await;
    attach_document(&mut a, ca, session, doc, "att-a").await;
    attach_document(&mut b, cb, session, doc, "att-b").await;

    let mut replica_a = seed_replica(&read_pool, doc).await;
    let mut replica_b = seed_replica(&read_pool, doc).await;

    // A leases the block and "edits" it — but in Suggest mode the edit lands as a
    // suggestion over exactly [0,5), and the content is unchanged on both sides.
    send(
        &mut a,
        ca,
        CommandBody::AcquireDocumentLease {
            lease: block_lease(doc, "p"),
            ttl_seconds: None,
        },
        "lease-a",
    )
    .await;
    let _lease_a = recv_lease_reply(&mut a).await.expect("A leases the block");

    send(
        &mut a,
        ca,
        CommandBody::MutateDocument {
            document_id: doc,
            mutation: edit_text("p", 0, 5, "final"),
        },
        "suggest-a",
    )
    .await;
    replica_a.merge(&recv_document_sync(&mut a).await).unwrap();
    replica_b.merge(&recv_document_sync(&mut b).await).unwrap();
    assert_eq!(
        blocks_text(&replica_a),
        "draft",
        "a suggestion changes no content"
    );
    assert_eq!(blocks_text(&replica_b), "draft");

    // The approver (B, a Controller) accepts it — no lease needed for a resolution.
    let suggestion_id = wait_for_suggestion(&read_pool, doc).await;
    send(
        &mut b,
        cb,
        CommandBody::MutateDocument {
            document_id: doc,
            mutation: DocumentMutation::AcceptSuggestion {
                suggestion_id: suggestion_id.clone(),
            },
        },
        "accept-b",
    )
    .await;

    // Accept applies exactly the annotated range; both replicas converge to the
    // byte-exact final content.
    replica_b.merge(&recv_document_sync(&mut b).await).unwrap();
    replica_a.merge(&recv_document_sync(&mut a).await).unwrap();
    let expected = render_document(
        "Org Doc",
        &[DocumentBlock::with_id(
            "p",
            BlockContent::Paragraph {
                text: "final".to_string(),
            },
        )],
    );
    assert_eq!(replica_a.render("Org Doc").unwrap(), expected);
    assert_eq!(replica_b.render("Org Doc").unwrap(), expected);
    assert_eq!(blocks_text(&replica_a), "final");

    read_pool.close().await;
}

/// The primary text of a single-paragraph replica (the block the tests edit).
fn blocks_text(replica: &DocumentReplica) -> String {
    match &replica.blocks().expect("blocks")[0].content {
        BlockContent::Paragraph { text } => text.clone(),
        other => panic!("expected a paragraph block, got {other:?}"),
    }
}
