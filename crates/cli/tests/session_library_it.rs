//! `codypendent session …` and `codypendent bundle …` client cores.
//!
//! Like `jsonl_it.rs` and `workflow_it.rs`, this drives the connected cores in
//! `codypendent_cli::commands` against a hand-rolled mock daemon built only
//! from `codypendent_protocol`'s framing — no `codypendentd` subprocess.
//!
//! The load-bearing assertions are the ones that would have caught the defect
//! this surface was written to fix: `MutateSessionLifecycle`, `ExportBundle`,
//! `ImportBundle` and `PutArtifact` all carry a `Controller` role floor, and a
//! client that does not bind it is refused invisibly. Every test below asserts
//! the bind crossed the wire before the command did, and the refusal paths
//! assert the CLI FAILS rather than reporting a success it did not get.

use std::time::Duration;

use codypendent_cli::connection::Connection;
use codypendent_protocol::{
    read_envelope, write_envelope, ArtifactId, ArtifactRef, BundleCollisionPolicy,
    BundleExportReceipt, BundleIdentityKind, BundleIdentityMapping, BundleImportProvenance,
    BundleImportReceipt, BundleInclusionPolicy, BundleManifest, BundleRedactionPolicy, ClientRole,
    CodypendentError, Command, CommandBody, CommandId, DaemonInstanceId, DataClassification,
    Envelope, Payload, ServerHello, SessionDeletionMode, SessionExportFormat, SessionExportOptions,
    SessionId, SessionLifecycleAction, SessionSearchPage, SessionSearchResult, SessionSearchScope,
    SessionSearchSource, SessionSummary, PROTOCOL_V1,
};
use tokio::net::{UnixListener, UnixStream};

struct MockSocket {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl MockSocket {
    fn bind() -> (Self, UnixListener) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).expect("bind mock socket");
        (Self { _dir: dir, path }, listener)
    }
}

fn expect_command(request: &Envelope) -> &Command {
    match &request.payload {
        Payload::Command(command) => command,
        other => panic!("expected a Command envelope, got {other:?}"),
    }
}

fn command_id_of(request: &Envelope) -> CommandId {
    expect_command(request).command_id
}

/// Handshake, then the `AttachSession` that binds the role. Returns nothing but
/// asserts the client asked for `Controller` — the whole point of the split.
async fn accept_handshake_and_role_bind(stream: &mut UnixStream) {
    let hello = read_envelope(stream)
        .await
        .expect("read ClientHello")
        .expect("connection open");
    assert!(matches!(hello.payload, Payload::ClientHello(_)));
    write_envelope(
        stream,
        &Envelope::reply_to(
            &hello,
            Payload::ServerHello(ServerHello {
                resume_token: None,
                selected_protocol: PROTOCOL_V1,
                daemon_version: "mock".to_string(),
                daemon_instance: DaemonInstanceId::new(),
                heartbeat_interval_ms: 15_000,
                build_id: String::new(),
            }),
        ),
    )
    .await
    .expect("write ServerHello");

    let attach = read_envelope(stream)
        .await
        .expect("read AttachSession")
        .expect("connection open");
    match &expect_command(&attach).body {
        CommandBody::AttachSession { requested_role, .. } => {
            assert_eq!(
                *requested_role,
                ClientRole::Controller,
                "the session-library and bundle commands all sit behind a Controller \
                 role floor; a client that does not bind it is refused invisibly"
            );
        }
        other => panic!("expected AttachSession, got {other:?}"),
    }
    write_envelope(
        stream,
        &Envelope::reply_to(
            &attach,
            Payload::CommandRejected(CodypendentError::new(
                "protocol.session-not-found",
                "unknown session",
                false,
            )),
        ),
    )
    .await
    .expect("write attach rejection");
}

fn summary(session_id: SessionId, title: &str) -> SessionSummary {
    SessionSummary {
        session_id,
        workspace_id: None,
        title: title.to_string(),
        state: "active".to_string(),
        updated_at: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
        internal: false,
        parent_session_id: None,
        parent_run_id: None,
        pinned: false,
        archived_at: None,
        repository_id: None,
        repository: None,
        workspace: None,
        last_activity_at: None,
        last_run_id: None,
        run_state: None,
    }
}

fn artifact_ref(bytes: &[u8], media_type: &str) -> ArtifactRef {
    use sha2::{Digest as _, Sha256};
    ArtifactRef {
        id: ArtifactId::new(),
        media_type: media_type.to_string(),
        byte_length: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        sensitivity: DataClassification::Confidential,
    }
}

/// Serve every `ReadArtifact` for `artifact` from `bytes`, one chunk per
/// request, until the client has read to EOF.
async fn serve_artifact_reads(stream: &mut UnixStream, artifact: &ArtifactRef, bytes: &[u8]) {
    // One chunk is enough for these fixtures: the reply below sets `eof`, so
    // the client stops after it.
    {
        let request = read_envelope(stream)
            .await
            .expect("read ReadArtifact")
            .expect("connection open");
        let (offset, expected_sha256) = match &expect_command(&request).body {
            CommandBody::ReadArtifact {
                artifact_id,
                offset,
                expected_sha256,
                ..
            } => {
                assert_eq!(*artifact_id, artifact.id);
                (*offset as usize, expected_sha256.clone())
            }
            other => panic!("expected ReadArtifact, got {other:?}"),
        };
        assert_eq!(
            expected_sha256, artifact.sha256,
            "every range request repeats the digest of the ref the client was given"
        );
        let end = bytes.len();
        let chunk = &bytes[offset.min(end)..end];
        write_envelope(
            stream,
            &Envelope::reply_to(
                &request,
                Payload::ArtifactChunk {
                    artifact_id: artifact.id,
                    offset: offset as u64,
                    bytes_base64: base64_encode(chunk),
                    eof: true,
                    sha256: artifact.sha256.clone(),
                },
            ),
        )
        .await
        .expect("write ArtifactChunk");
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn accept(listener: &UnixListener) -> UnixStream {
    let (stream, _addr) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("mock accepted a connection in time")
        .expect("accept");
    stream
}

#[tokio::test]
async fn session_search_binds_controller_and_renders_the_ranked_page() {
    let (socket, listener) = MockSocket::bind();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;

        let request = read_envelope(&mut stream)
            .await
            .expect("read SearchSessions")
            .expect("connection open");
        let command_id = command_id_of(&request);
        match &expect_command(&request).body {
            CommandBody::SearchSessions { query } => {
                assert_eq!(query.query, "migration");
                assert!(query.cursor.is_none(), "the first page carries no cursor");
                assert_eq!(query.limit, 5);
            }
            other => panic!("expected SearchSessions, got {other:?}"),
        }
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::SessionSearchResults {
                    command_id,
                    page: SessionSearchPage {
                        items: vec![
                            SessionSearchResult {
                                session: summary(session_id, "database migration"),
                                source: SessionSearchSource::Transcript,
                                scope: SessionSearchScope::User,
                                stable_identity: "s1".to_string(),
                                deep_link: codypendent_protocol::SessionDeepLink::Session {
                                    session_id,
                                },
                                score: 0.9,
                                excerpt: Some("ran the 0049 migration".to_string()),
                            },
                            SessionSearchResult {
                                session: summary(SessionId::new(), "unrelated but titled"),
                                source: SessionSearchSource::Title,
                                scope: SessionSearchScope::User,
                                stable_identity: "s2".to_string(),
                                deep_link: codypendent_protocol::SessionDeepLink::Session {
                                    session_id,
                                },
                                score: 0.2,
                                // No excerpt: this hit matched on its title.
                                excerpt: None,
                            },
                        ],
                        next_cursor: None,
                    },
                },
            ),
        )
        .await
        .expect("write SessionSearchResults");
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut out: Vec<u8> = Vec::new();
    codypendent_cli::commands::session_search_over_connection(
        &mut conn,
        "migration",
        Some(5),
        None,
        false,
        &mut out,
    )
    .await
    .expect("session search");
    server.await.expect("mock server task");

    let printed = String::from_utf8(out).expect("utf8");
    assert!(printed.contains("database migration"), "{printed}");
    assert!(
        printed.contains("ran the 0049 migration"),
        "the ranked hit's excerpt is shown: {printed}"
    );
    // An absent excerpt stays absent — no empty quote is invented for the
    // title-only hit.
    assert_eq!(
        printed.matches('\u{201c}').count(),
        1,
        "only the hit that HAS an excerpt gets one printed: {printed}"
    );
}

#[tokio::test]
async fn a_refused_search_fails_instead_of_printing_an_empty_result_set() {
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;
        let request = read_envelope(&mut stream)
            .await
            .expect("read SearchSessions")
            .expect("connection open");
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::CommandRejected(CodypendentError::new(
                    "session-library.query-failed",
                    "the session library could not be queried",
                    true,
                )),
            ),
        )
        .await
        .expect("write rejection");
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut out: Vec<u8> = Vec::new();
    let error = codypendent_cli::commands::session_search_over_connection(
        &mut conn, "x", None, None, false, &mut out,
    )
    .await
    .expect_err("a refused search must fail");
    server.await.expect("mock server task");

    assert!(
        error.to_string().contains("session-library.query-failed"),
        "the daemon's own code reaches the operator: {error}"
    );
    assert!(
        out.is_empty(),
        "nothing is printed for a search that did not happen"
    );
}

#[tokio::test]
async fn pin_reports_the_daemons_projection_not_the_clients_assumption() {
    let (socket, listener) = MockSocket::bind();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;
        let request = read_envelope(&mut stream)
            .await
            .expect("read MutateSessionLifecycle")
            .expect("connection open");
        let command_id = command_id_of(&request);
        match &expect_command(&request).body {
            CommandBody::MutateSessionLifecycle {
                session_id: target,
                action,
            } => {
                assert_eq!(*target, session_id);
                assert!(matches!(action, SessionLifecycleAction::Pin));
            }
            other => panic!("expected MutateSessionLifecycle, got {other:?}"),
        }
        // The daemon renamed it too (another client did). The CLI must report
        // THIS, not the state it assumed.
        let mut applied = summary(session_id, "the daemon's title");
        applied.pinned = true;
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::SessionLifecycleApplied {
                    command_id,
                    session: applied,
                },
            ),
        )
        .await
        .expect("write SessionLifecycleApplied");
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut out: Vec<u8> = Vec::new();
    let artifact = codypendent_cli::commands::session_lifecycle_over_connection(
        &mut conn,
        session_id,
        SessionLifecycleAction::Pin,
        "pinned",
        &mut out,
    )
    .await
    .expect("pin");
    server.await.expect("mock server task");

    assert!(artifact.is_none());
    let printed = String::from_utf8(out).expect("utf8");
    assert!(printed.contains("the daemon's title"), "{printed}");
    assert!(printed.contains("pinned=true"), "{printed}");
}

#[tokio::test]
async fn delete_reports_a_tombstone_as_a_tombstone() {
    let (socket, listener) = MockSocket::bind();
    let session_id = SessionId::new();
    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;
        let request = read_envelope(&mut stream)
            .await
            .expect("read MutateSessionLifecycle")
            .expect("connection open");
        let command_id = command_id_of(&request);
        match &expect_command(&request).body {
            CommandBody::MutateSessionLifecycle { action, .. } => assert!(matches!(
                action,
                SessionLifecycleAction::Delete {
                    mode: SessionDeletionMode::RetentionPolicy
                }
            )),
            other => panic!("expected MutateSessionLifecycle, got {other:?}"),
        }
        // The client asked for the retention policy; the daemon decided that
        // means a tombstone. The CLI must say so.
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::SessionDeleted {
                    command_id,
                    session_id,
                    tombstoned: true,
                },
            ),
        )
        .await
        .expect("write SessionDeleted");
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut out: Vec<u8> = Vec::new();
    codypendent_cli::commands::session_lifecycle_over_connection(
        &mut conn,
        session_id,
        SessionLifecycleAction::Delete {
            mode: SessionDeletionMode::RetentionPolicy,
        },
        "deleted",
        &mut out,
    )
    .await
    .expect("delete");
    server.await.expect("mock server task");

    let printed = String::from_utf8(out).expect("utf8");
    assert!(printed.contains("tombstoned"), "{printed}");
}

#[tokio::test]
async fn a_role_denied_lifecycle_mutation_fails_loudly() {
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;
        let request = read_envelope(&mut stream)
            .await
            .expect("read MutateSessionLifecycle")
            .expect("connection open");
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::CommandRejected(CodypendentError::new(
                    "protocol.role-denied",
                    "this role may not mutate a session lifecycle",
                    false,
                )),
            ),
        )
        .await
        .expect("write rejection");
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut out: Vec<u8> = Vec::new();
    let error = codypendent_cli::commands::session_lifecycle_over_connection(
        &mut conn,
        SessionId::new(),
        SessionLifecycleAction::Archive,
        "archived",
        &mut out,
    )
    .await
    .expect_err("a role-denied mutation must fail, never be swallowed");
    server.await.expect("mock server task");

    assert!(
        error.to_string().contains("protocol.role-denied"),
        "{error}"
    );
    assert!(
        out.is_empty(),
        "nothing is reported for a mutation that did not happen"
    );
}

#[tokio::test]
async fn session_export_returns_the_artifact_for_the_caller_to_download() {
    let (socket, listener) = MockSocket::bind();
    let session_id = SessionId::new();
    let body = b"# transcript\n\nhello".to_vec();
    let expected = artifact_ref(&body, "text/markdown");
    let expected_for_server = expected.clone();
    let body_for_server = body.clone();
    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;
        let request = read_envelope(&mut stream)
            .await
            .expect("read MutateSessionLifecycle")
            .expect("connection open");
        let command_id = command_id_of(&request);
        match &expect_command(&request).body {
            CommandBody::MutateSessionLifecycle { action, .. } => match action {
                SessionLifecycleAction::Export { options } => {
                    assert_eq!(options.format, SessionExportFormat::Markdown);
                    assert!(
                        !options.include_artifacts && !options.include_internal_sessions,
                        "an export never widens by default"
                    );
                }
                other => panic!("expected Export, got {other:?}"),
            },
            other => panic!("expected MutateSessionLifecycle, got {other:?}"),
        }
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::SessionExported {
                    command_id,
                    artifact: expected_for_server.clone(),
                },
            ),
        )
        .await
        .expect("write SessionExported");
        serve_artifact_reads(&mut stream, &expected_for_server, &body_for_server).await;
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut out: Vec<u8> = Vec::new();
    let artifact = codypendent_cli::commands::session_lifecycle_over_connection(
        &mut conn,
        session_id,
        SessionLifecycleAction::Export {
            options: SessionExportOptions {
                format: SessionExportFormat::Markdown,
                include_artifacts: false,
                include_internal_sessions: false,
            },
        },
        "export",
        &mut out,
    )
    .await
    .expect("export")
    .expect("an export names an artifact");
    assert_eq!(artifact.sha256, expected.sha256);
    server.abort();
}

#[tokio::test]
async fn bundle_export_writes_verified_bytes_and_reports_the_redaction_summary() {
    let (socket, listener) = MockSocket::bind();
    let dir = tempfile::tempdir().expect("tempdir");
    let out_path = dir.path().join("nested").join("bundle.tar");
    let archive = b"bundle-bytes-0123456789".to_vec();
    let bundle_ref = artifact_ref(&archive, "application/vnd.codypendent.bundle");
    let bundle_for_server = bundle_ref.clone();
    let archive_for_server = archive.clone();
    let session_id = SessionId::new();

    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;
        let request = read_envelope(&mut stream)
            .await
            .expect("read ExportBundle")
            .expect("connection open");
        let command_id = command_id_of(&request);
        match &expect_command(&request).body {
            CommandBody::ExportBundle { request } => {
                assert_eq!(request.source_session_ids, vec![session_id]);
                assert!(request.inclusion.transcript_events);
                // Every switch the caller did not set stays closed on the wire.
                assert!(!request.inclusion.environment_diagnostics);
                assert!(!request.inclusion.patches);
                assert_eq!(request.redaction_policy, BundleRedactionPolicy::SupportSafe);
            }
            other => panic!("expected ExportBundle, got {other:?}"),
        }
        let mut manifest = BundleManifest {
            format_version: 1,
            created_at: chrono::Utc::now(),
            source_session_ids: vec![session_id],
            inclusion: BundleInclusionPolicy {
                transcript_events: true,
                ..Default::default()
            },
            redaction_policy: BundleRedactionPolicy::SupportSafe,
            redaction_summary: Default::default(),
            entries: Vec::new(),
            manifest_sha256: "ab".repeat(32),
        };
        manifest.redaction_summary.values_replaced = 3;
        manifest.redaction_summary.credentials_omitted = 1;
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::BundleExported {
                    command_id,
                    receipt: BundleExportReceipt {
                        bundle: bundle_for_server.clone(),
                        manifest,
                    },
                },
            ),
        )
        .await
        .expect("write BundleExported");
        serve_artifact_reads(&mut stream, &bundle_for_server, &archive_for_server).await;
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut log: Vec<u8> = Vec::new();
    codypendent_cli::commands::bundle_export_over_connection(
        &mut conn,
        vec![session_id],
        BundleInclusionPolicy {
            transcript_events: true,
            ..Default::default()
        },
        BundleRedactionPolicy::SupportSafe,
        &out_path,
        &mut log,
    )
    .await
    .expect("bundle export");
    server.await.expect("mock server task");

    assert_eq!(std::fs::read(&out_path).expect("archive written"), archive);
    let printed = String::from_utf8(log).expect("utf8");
    assert!(printed.contains("3 values replaced"), "{printed}");
    assert!(printed.contains("1 credentials omitted"), "{printed}");
}

#[tokio::test]
async fn a_corrupt_bundle_preserves_an_existing_export() {
    let (socket, listener) = MockSocket::bind();
    let dir = tempfile::tempdir().expect("tempdir");
    let out_path = dir.path().join("bundle.tar");
    std::fs::write(&out_path, b"keep the previous export").unwrap();
    // The ref promises one digest; the daemon then serves different bytes.
    let promised = artifact_ref(b"the promised bytes", "application/vnd.codypendent.bundle");
    let promised_for_server = promised.clone();

    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;
        let request = read_envelope(&mut stream)
            .await
            .expect("read ExportBundle")
            .expect("connection open");
        let command_id = command_id_of(&request);
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &request,
                Payload::BundleExported {
                    command_id,
                    receipt: BundleExportReceipt {
                        bundle: promised_for_server.clone(),
                        manifest: BundleManifest {
                            format_version: 1,
                            created_at: chrono::Utc::now(),
                            source_session_ids: Vec::new(),
                            inclusion: Default::default(),
                            redaction_policy: BundleRedactionPolicy::Standard,
                            redaction_summary: Default::default(),
                            entries: Vec::new(),
                            manifest_sha256: "cd".repeat(32),
                        },
                    },
                },
            ),
        )
        .await
        .expect("write BundleExported");
        let corrupted = b"CORRUPTED bytes!!!";
        assert_eq!(corrupted.len() as u64, promised_for_server.byte_length);
        serve_artifact_reads(&mut stream, &promised_for_server, corrupted).await;
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut log: Vec<u8> = Vec::new();
    let error = codypendent_cli::commands::bundle_export_over_connection(
        &mut conn,
        Vec::new(),
        BundleInclusionPolicy::default(),
        BundleRedactionPolicy::Standard,
        &out_path,
        &mut log,
    )
    .await
    .expect_err("a digest mismatch must fail");
    server.await.expect("mock server task");

    assert!(error.to_string().contains("digest mismatch"), "{error}");
    assert_eq!(
        std::fs::read(&out_path).unwrap(),
        b"keep the previous export"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert!(log.is_empty(), "a failed export never reports success");
}

#[tokio::test]
async fn bundle_import_uploads_then_imports_and_prints_the_identity_remapping() {
    let (socket, listener) = MockSocket::bind();
    let archive = b"an archive".to_vec();
    let stored = artifact_ref(&archive, "application/vnd.codypendent.bundle");
    let stored_for_server = stored.clone();
    let archive_for_server = archive.clone();
    let imported = SessionId::new();

    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;

        let upload = read_envelope(&mut stream)
            .await
            .expect("read PutArtifact")
            .expect("connection open");
        let upload_command_id = command_id_of(&upload);
        match &expect_command(&upload).body {
            CommandBody::PutArtifact {
                bytes_base64,
                sensitivity,
                ..
            } => {
                assert_eq!(*bytes_base64, base64_encode(&archive_for_server));
                assert_eq!(
                    *sensitivity,
                    DataClassification::Confidential,
                    "a round-trip must not downgrade what the exporter classified"
                );
            }
            other => panic!("expected PutArtifact, got {other:?}"),
        }
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &upload,
                Payload::ArtifactStored {
                    command_id: upload_command_id,
                    artifact: stored_for_server.clone(),
                },
            ),
        )
        .await
        .expect("write ArtifactStored");

        let import = read_envelope(&mut stream)
            .await
            .expect("read ImportBundle")
            .expect("connection open");
        let import_command_id = command_id_of(&import);
        match &expect_command(&import).body {
            CommandBody::ImportBundle { request } => {
                assert_eq!(
                    request.bundle.id, stored_for_server.id,
                    "the import names exactly the ref the upload minted"
                );
                assert_eq!(request.collision_policy, BundleCollisionPolicy::Remap);
            }
            other => panic!("expected ImportBundle, got {other:?}"),
        }
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &import,
                Payload::BundleImported {
                    command_id: import_command_id,
                    receipt: BundleImportReceipt {
                        provenance: BundleImportProvenance {
                            bundle_sha256: stored_for_server.sha256.clone(),
                            manifest_sha256: "ef".repeat(32),
                            imported_at: chrono::Utc::now(),
                            source_session_ids: Vec::new(),
                        },
                        identity_mappings: vec![BundleIdentityMapping {
                            kind: BundleIdentityKind::Session,
                            source_id: "source-1".to_string(),
                            local_id: imported.to_string(),
                            provenance: BundleImportProvenance {
                                bundle_sha256: stored_for_server.sha256.clone(),
                                manifest_sha256: "ef".repeat(32),
                                imported_at: chrono::Utc::now(),
                                source_session_ids: Vec::new(),
                            },
                        }],
                        imported_session_ids: vec![imported],
                        skipped_entries: 2,
                    },
                },
            ),
        )
        .await
        .expect("write BundleImported");
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut log: Vec<u8> = Vec::new();
    codypendent_cli::commands::bundle_import_over_connection(
        &mut conn,
        std::path::Path::new("/tmp/in.tar"),
        archive,
        BundleCollisionPolicy::Remap,
        &mut log,
    )
    .await
    .expect("bundle import");
    server.await.expect("mock server task");

    let printed = String::from_utf8(log).expect("utf8");
    assert!(printed.contains("imported 1 session(s)"), "{printed}");
    assert!(printed.contains("2 entries skipped"), "{printed}");
    assert!(printed.contains("source-1"), "{printed}");
}

#[tokio::test]
async fn a_refused_upload_stops_the_import_before_it_names_a_bundle() {
    let (socket, listener) = MockSocket::bind();
    let server = tokio::spawn(async move {
        let mut stream = accept(&listener).await;
        accept_handshake_and_role_bind(&mut stream).await;
        let upload = read_envelope(&mut stream)
            .await
            .expect("read PutArtifact")
            .expect("connection open");
        write_envelope(
            &mut stream,
            &Envelope::reply_to(
                &upload,
                Payload::CommandRejected(CodypendentError::new(
                    "artifact.store-failed",
                    "could not store the uploaded artifact",
                    true,
                )),
            ),
        )
        .await
        .expect("write rejection");
        // Nothing else may arrive: a failed upload must not be followed by an
        // ImportBundle naming a ref that does not exist.
        let next = tokio::time::timeout(Duration::from_millis(300), read_envelope(&mut stream))
            .await
            .map(|r| r.expect("read").is_some())
            .unwrap_or(false);
        assert!(!next, "no ImportBundle may follow a refused upload");
    });

    let mut conn = Connection::connect(&socket.path).await.expect("connect");
    let mut log: Vec<u8> = Vec::new();
    let error = codypendent_cli::commands::bundle_import_over_connection(
        &mut conn,
        std::path::Path::new("/tmp/in.tar"),
        b"bytes".to_vec(),
        BundleCollisionPolicy::Reject,
        &mut log,
    )
    .await
    .expect_err("a refused upload must fail the import");
    server.await.expect("mock server task");

    assert!(
        error.to_string().contains("artifact.store-failed"),
        "{error}"
    );
    assert!(log.is_empty());
}
