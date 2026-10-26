//! Shared, acyclic council service.
//!
//! Both the CLI and the daemon-side agent runtime use this crate. It owns the
//! validated definition store, durable result store, and the daemon-protocol
//! runner; neither depends on the other to operate a council.

pub mod artifacts;
pub mod connection;
mod roles;
mod service;

pub use service::*;

mod commands {
    use std::time::Duration;

    use anyhow::Context as _;
    use codypendent_protocol::discovery::RuntimePaths;
    use codypendent_protocol::{Catchup, Envelope, Payload};

    /// Ensure the daemon socket exists. This lower-layer variant deliberately
    /// knows only the public single-binary launch contract.
    pub async fn ensure_daemon(paths: &RuntimePaths) -> anyhow::Result<()> {
        if tokio::net::UnixStream::connect(&paths.socket_path)
            .await
            .is_ok()
        {
            return Ok(());
        }
        paths.ensure_directories()?;
        let executable = std::env::current_exe().context("resolving codypendent executable")?;
        let log_path = paths.log_dir.join("daemon.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let stderr = log.try_clone()?;
        let mut command = std::process::Command::new(executable);
        command
            .arg("__daemon")
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(stderr);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        command.spawn().context("spawning codypendent daemon")?;
        for _ in 0..50 {
            if tokio::net::UnixStream::connect(&paths.socket_path)
                .await
                .is_ok()
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!(
            "daemon did not become ready within 5 seconds; check {}",
            log_path.display()
        )
    }

    pub fn expect_catchup(reply: Envelope) -> anyhow::Result<Catchup> {
        match reply.payload {
            Payload::Catchup { catchup } => Ok(catchup),
            Payload::CommandRejected(error) => {
                anyhow::bail!("AttachSession rejected: {} ({})", error.message, error.code)
            }
            other => anyhow::bail!("unexpected reply to AttachSession: {other:?}"),
        }
    }
}
