//! Protocol server behaviour over a real Unix socket: ping, status, version
//! rejection, and graceful shutdown with socket cleanup.

use std::time::Duration;

use codypendent_daemon::{db, instance, server};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, ClientId, Envelope, Payload, ProtocolVersion,
};
use tokio::net::UnixStream;

async fn roundtrip(stream: &mut UnixStream, request: &Envelope) -> Envelope {
    write_envelope(stream, request).await.expect("write frame");
    read_envelope(stream)
        .await
        .expect("read frame")
        .expect("server must reply")
}

#[tokio::test]
async fn ping_status_shutdown_over_socket() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
    paths.ensure_directories().expect("create directories");

    let pool = db::open_database(&paths.data_dir.join("codypendent.db"))
        .await
        .expect("open db");
    let boot = instance::record_boot(&pool).await.expect("record boot");
    let server_task = tokio::spawn(server::run(pool, paths.clone(), boot));

    // Wait for the server to bind.
    let mut stream = loop {
        match UnixStream::connect(&paths.socket_path).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };

    let client_id = ClientId::new();

    let reply = roundtrip(&mut stream, &Envelope::request(client_id, Payload::Ping)).await;
    assert!(matches!(reply.payload, Payload::Pong));

    let reply = roundtrip(
        &mut stream,
        &Envelope::request(client_id, Payload::DaemonStatusRequest),
    )
    .await;
    match reply.payload {
        Payload::DaemonStatusResponse(status) => {
            assert_eq!(status.boot_count, 1);
            assert_eq!(status.session_count, 0);
        }
        other => panic!("expected status response, got {other:?}"),
    }

    // An incompatible major version must produce a structured error.
    let mut bad = Envelope::request(client_id, Payload::Ping);
    bad.protocol_version = ProtocolVersion {
        major: 99,
        minor: 0,
    };
    let reply = roundtrip(&mut stream, &bad).await;
    match reply.payload {
        Payload::Error(error) => assert_eq!(error.code, "protocol.incompatible-version"),
        other => panic!("expected version error, got {other:?}"),
    }

    // An unknown (future, additive) payload must be rejected structurally,
    // and the connection must remain usable afterwards.
    let mut future_value =
        serde_json::to_value(Envelope::request(client_id, Payload::Ping)).expect("serialize");
    future_value["payload"] = serde_json::json!({ "type": "PayloadFromTheFuture" });
    let future: Envelope = serde_json::from_value(future_value).expect("parse");
    let reply = roundtrip(&mut stream, &future).await;
    match reply.payload {
        Payload::Error(error) => assert_eq!(error.code, "protocol.unsupported-payload"),
        other => panic!("expected unsupported-payload error, got {other:?}"),
    }
    let reply = roundtrip(&mut stream, &Envelope::request(client_id, Payload::Ping)).await;
    assert!(
        matches!(reply.payload, Payload::Pong),
        "connection must survive an unknown payload"
    );

    let reply = roundtrip(
        &mut stream,
        &Envelope::request(client_id, Payload::Shutdown),
    )
    .await;
    assert!(matches!(reply.payload, Payload::ShutdownAck));

    let joined = tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .expect("server must stop within 5 seconds")
        .expect("server task must not panic");
    joined.expect("server must stop cleanly");
    assert!(!paths.socket_path.exists(), "socket file must be removed");
    assert!(!paths.pid_path.exists(), "pidfile must be removed");
}
