//! Daemon process discovery.
//!
//! Discovery is part of the protocol contract: every client must resolve the
//! same socket path as the daemon, with no coordination other than this code.
//!
//! Data layout (override the root with `CODYPENDENT_DATA_DIR`):
//!
//! ```text
//! ~/.local/share/codypendent/
//! ├── codypendent.db        (daemon-owned; clients never open it)
//! ├── logs/
//! │   └── daemon.log
//! └── artifacts/            (content-addressed store, Phase 1)
//! ```
//!
//! Socket resolution order (Unix sockets are limited to roughly 104–108
//! bytes of path, so the socket cannot always live under the data dir):
//!
//! 1. `CODYPENDENT_SOCKET` — explicit override.
//! 2. `<CODYPENDENT_DATA_DIR>/run/daemon.sock` — when the data dir is
//!    overridden, everything stays under it (test isolation).
//! 3. `$XDG_RUNTIME_DIR/codypendent/daemon.sock` — short, user-private,
//!    cleaned on logout.
//! 4. `<data dir>/run/daemon.sock` — fallback.
//!
//! The pidfile always sits next to the socket.

use std::path::{Path, PathBuf};

/// Conservative bound below the platform SUN_LEN limits (104 on macOS/BSD,
/// 108 on Linux).
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub data_dir: PathBuf,
    pub run_dir: PathBuf,
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub log_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("cannot determine a home directory for the current user")]
    NoHomeDirectory,
    #[error(
        "socket path `{path}` is {length} bytes; Unix domain socket paths are limited to \
         roughly 104-108 bytes. Set CODYPENDENT_SOCKET to a shorter path (for example under \
         /tmp) or use a shorter CODYPENDENT_DATA_DIR."
    )]
    SocketPathTooLong { path: String, length: usize },
}

impl RuntimePaths {
    /// Resolve paths from the environment (see module docs for the order).
    pub fn resolve() -> Result<Self, DiscoveryError> {
        let data_dir_override = std::env::var_os("CODYPENDENT_DATA_DIR").map(PathBuf::from);
        let data_dir = match &data_dir_override {
            Some(dir) => dir.clone(),
            None => directories::ProjectDirs::from("", "", "codypendent")
                .ok_or(DiscoveryError::NoHomeDirectory)?
                .data_dir()
                .to_path_buf(),
        };

        let socket_path = if let Some(socket) = std::env::var_os("CODYPENDENT_SOCKET") {
            PathBuf::from(socket)
        } else if data_dir_override.is_some() {
            data_dir.join("run").join("daemon.sock")
        } else if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(runtime_dir)
                .join("codypendent")
                .join("daemon.sock")
        } else {
            data_dir.join("run").join("daemon.sock")
        };

        let paths = Self::with_socket(data_dir, socket_path);
        paths.validate_socket_path()?;
        Ok(paths)
    }

    /// Derive every runtime path from an explicit data directory (tests and
    /// embedded use). The socket lives under `<data_dir>/run/`.
    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        let socket_path = data_dir.join("run").join("daemon.sock");
        Self::with_socket(data_dir, socket_path)
    }

    fn with_socket(data_dir: PathBuf, socket_path: PathBuf) -> Self {
        let run_dir = socket_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.join("run"));
        Self {
            pid_path: run_dir.join("daemon.pid"),
            log_dir: data_dir.join("logs"),
            run_dir,
            socket_path,
            data_dir,
        }
    }

    /// Fail early, with an actionable error, instead of letting `bind` fail
    /// with an opaque SUN_LEN error.
    pub fn validate_socket_path(&self) -> Result<(), DiscoveryError> {
        let length = self.socket_path.as_os_str().len();
        if length > MAX_SOCKET_PATH_BYTES {
            return Err(DiscoveryError::SocketPathTooLong {
                path: self.socket_path.display().to_string(),
                length,
            });
        }
        Ok(())
    }

    /// Create the data, run, and log directories. On Unix the directories are
    /// restricted to the owning user (0o700) because the socket grants daemon
    /// access.
    pub fn ensure_directories(&self) -> std::io::Result<()> {
        for dir in [&self.data_dir, &self.run_dir, &self.log_dir] {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }
}
