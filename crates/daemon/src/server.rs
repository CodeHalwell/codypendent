//! Unix-domain-socket protocol server.
//!
//! Phase 0 serves Ping, DaemonStatusRequest, and Shutdown. The accept loop,
//! per-connection framing, version check, and graceful-shutdown plumbing are
//! the skeleton every later payload handler plugs into.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, DaemonStatus, Envelope, Payload, ProtocolError, PROTOCOL_V1,
};
use sqlx::SqlitePool;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::instance::InstanceRecord;
use crate::ledger;

pub struct ServerState {
    pub pool: SqlitePool,
    pub paths: RuntimePaths,
    pub instance: InstanceRecord,
    pub started_at: DateTime<Utc>,
    pub shutdown: watch::Sender<bool>,
}

/// Bind the socket, write the pidfile, and serve until Shutdown or SIGTERM /
/// SIGINT. Removes the socket and pidfile on exit.
pub async fn run(
    pool: SqlitePool,
    paths: RuntimePaths,
    instance: InstanceRecord,
) -> anyhow::Result<()> {
    prepare_socket(&paths).await?;
    let listener = UnixListener::bind(&paths.socket_path)?;
    std::fs::write(&paths.pid_path, std::process::id().to_string())?;
    info!(socket = %paths.socket_path.display(), pid = std::process::id(), "daemon listening");

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let state = Arc::new(ServerState {
        pool,
        paths: paths.clone(),
        instance,
        started_at: Utc::now(),
        shutdown: shutdown_tx,
    });

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("shutdown requested via protocol");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT received");
                break;
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, state).await {
                                warn!(error = %e, "connection ended with error");
                            }
                        });
                    }
                    Err(e) => error!(error = %e, "accept failed"),
                }
            }
        }
    }

    let _ = std::fs::remove_file(&paths.socket_path);
    let _ = std::fs::remove_file(&paths.pid_path);
    info!("daemon stopped");
    Ok(())
}

/// Refuse to start if a live daemon already owns the socket; remove the
/// socket file if it is stale (bind would otherwise fail with AddrInUse).
async fn prepare_socket(paths: &RuntimePaths) -> anyhow::Result<()> {
    paths.validate_socket_path()?;
    if paths.socket_path.exists() {
        match UnixStream::connect(&paths.socket_path).await {
            Ok(_) => anyhow::bail!(
                "another daemon is already listening on {}",
                paths.socket_path.display()
            ),
            Err(_) => {
                warn!(socket = %paths.socket_path.display(), "removing stale socket");
                std::fs::remove_file(&paths.socket_path)?;
            }
        }
    }
    Ok(())
}

async fn handle_connection(mut stream: UnixStream, state: Arc<ServerState>) -> anyhow::Result<()> {
    while let Some(request) = read_envelope(&mut stream).await? {
        if !request.protocol_version.compatible_with(&PROTOCOL_V1) {
            let reply = Envelope::reply_to(
                &request,
                Payload::Error(ProtocolError {
                    code: "protocol.incompatible-version".to_string(),
                    message: format!(
                        "daemon speaks {PROTOCOL_V1}, client sent {}",
                        request.protocol_version
                    ),
                    retryable: false,
                }),
            );
            write_envelope(&mut stream, &reply).await?;
            continue;
        }

        let reply_payload = match &request.payload {
            Payload::Ping => Payload::Pong,
            Payload::DaemonStatusRequest => Payload::DaemonStatusResponse(status(&state).await?),
            Payload::Shutdown => Payload::ShutdownAck,
            other => Payload::Error(ProtocolError {
                code: "protocol.unsupported-payload".to_string(),
                message: format!("payload not handled in this phase: {other:?}"),
                retryable: false,
            }),
        };

        let is_shutdown = matches!(request.payload, Payload::Shutdown);
        write_envelope(&mut stream, &Envelope::reply_to(&request, reply_payload)).await?;
        if is_shutdown {
            let _ = state.shutdown.send(true);
            break;
        }
    }
    Ok(())
}

async fn status(state: &ServerState) -> anyhow::Result<DaemonStatus> {
    let uptime = Utc::now()
        .signed_duration_since(state.started_at)
        .num_seconds()
        .max(0) as u64;
    Ok(DaemonStatus {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_V1,
        instance_id: state.instance.instance_id,
        pid: std::process::id(),
        started_at: state.started_at,
        uptime_seconds: uptime,
        boot_count: state.instance.boot_count,
        database_path: state
            .paths
            .data_dir
            .join("codypendent.db")
            .display()
            .to_string(),
        socket_path: state.paths.socket_path.display().to_string(),
        session_count: ledger::session_count(&state.pool).await?,
    })
}
