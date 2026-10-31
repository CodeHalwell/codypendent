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
//!
//! Config layout (override the root with `CODYPENDENT_CONFIG_DIR`):
//!
//! ```text
//! ~/.config/codypendent/
//! └── policy.toml           (global, trusted policy overlay — see policy::)
//! ```
//!
//! `config_dir` resolves independently of `data_dir` (XDG separates config
//! from data), except in the test-isolation case: when `CODYPENDENT_DATA_DIR`
//! is set but `CODYPENDENT_CONFIG_DIR` is not, `config_dir` falls under the
//! overridden data dir too, so an isolated test never reads the real user's
//! config directory.
//!
//! Hosts embedding their own runtime select `AGENT_RUNTIME_PRODUCT=son-of-anton`
//! or call [`RuntimePaths::resolve_for`] explicitly. That profile uses the
//! `son-of-anton` namespace and only `SON_OF_ANTON_DATA_DIR`,
//! `SON_OF_ANTON_CONFIG_DIR`, and `SON_OF_ANTON_SOCKET` overrides. Inherited
//! `CODYPENDENT_*` paths never attach a private host runtime to Codypendent.

use std::path::{Path, PathBuf};

/// Conservative bound below the platform SUN_LEN limits (104 on macOS/BSD,
/// 108 on Linux).
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

/// The host whose private configuration, credentials, state, and daemon are used.
/// Selecting a product changes discovery only; it does not change wire types.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuntimeProduct {
    #[default]
    Codypendent,
    SonOfAnton,
}

impl RuntimeProduct {
    /// Select the host for a CLI or daemon invocation, failing closed on an
    /// unknown selector rather than opening the default product's state.
    pub fn from_env() -> Result<Self, DiscoveryError> {
        match std::env::var("AGENT_RUNTIME_PRODUCT") {
            Ok(selector) => Self::from_selector(Some(&selector)),
            Err(std::env::VarError::NotPresent) => Ok(Self::Codypendent),
            Err(std::env::VarError::NotUnicode(_)) => Err(DiscoveryError::UnknownRuntimeProduct),
        }
    }

    /// Stable directory namespace shared with the Node discovery SDK.
    pub fn namespace(self) -> &'static str {
        match self {
            Self::Codypendent => "codypendent",
            Self::SonOfAnton => "son-of-anton",
        }
    }

    /// Prefix for this host's path overrides when launching a child process.
    pub fn environment_prefix(self) -> &'static str {
        match self {
            Self::Codypendent => "CODYPENDENT",
            Self::SonOfAnton => "SON_OF_ANTON",
        }
    }

    fn from_selector(value: Option<&str>) -> Result<Self, DiscoveryError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("codypendent") => Ok(Self::Codypendent),
            Some("son-of-anton") => Ok(Self::SonOfAnton),
            Some(_) => Err(DiscoveryError::UnknownRuntimeProduct),
        }
    }
}

/// An environment override, or `None` when the variable is unset, empty, or
/// whitespace-only. An empty override is a misconfiguration, not a real path.
fn non_empty_env(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
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
        "AGENT_RUNTIME_PRODUCT must be codypendent or son-of-anton; refusing to use another product's state"
    )]
    UnknownRuntimeProduct,
    #[error(
        "socket path `{path}` is {length} bytes; Unix domain socket paths are limited to \
         roughly 104-108 bytes. Set CODYPENDENT_SOCKET or SON_OF_ANTON_SOCKET for the selected \
         product to a shorter path (for example under /tmp), or shorten its DATA_DIR override."
    )]
    SocketPathTooLong { path: String, length: usize },
}

impl RuntimePaths {
    /// Resolve the selected host's paths. With no `AGENT_RUNTIME_PRODUCT`, the
    /// original Codypendent defaults and overrides remain unchanged.
    pub fn resolve() -> Result<Self, DiscoveryError> {
        Self::resolve_for(RuntimeProduct::from_env()?)
    }

    /// Resolve one host explicitly, ignoring `AGENT_RUNTIME_PRODUCT` and all
    /// other hosts' path overrides. Useful for optional, explicit attachment.
    pub fn resolve_for(product: RuntimeProduct) -> Result<Self, DiscoveryError> {
        Self::resolve_with_env(product, non_empty_env)
    }

    fn resolve_with_env(
        product: RuntimeProduct,
        env: impl Fn(&str) -> Option<PathBuf>,
    ) -> Result<Self, DiscoveryError> {
        // An env var set to an empty (or whitespace-only) string is treated as
        // unset — otherwise `CODYPENDENT_DATA_DIR=""` would make the data dir the
        // empty path and yield relative socket paths like `run/daemon.sock`.
        let data_dir_override = env(&format!("{}_DATA_DIR", product.environment_prefix()));
        let data_dir = match &data_dir_override {
            Some(dir) => dir.clone(),
            None => directories::ProjectDirs::from("", "", product.namespace())
                .ok_or(DiscoveryError::NoHomeDirectory)?
                .data_dir()
                .to_path_buf(),
        };

        let socket_path =
            if let Some(socket) = env(&format!("{}_SOCKET", product.environment_prefix())) {
                socket
            } else if data_dir_override.is_some() {
                data_dir.join("run").join("daemon.sock")
            } else if let Some(runtime_dir) = env("XDG_RUNTIME_DIR") {
                runtime_dir.join(product.namespace()).join("daemon.sock")
            } else {
                data_dir.join("run").join("daemon.sock")
            };

        // Mirrors the socket-path precedent above: an explicit override wins;
        // otherwise, if the data dir itself was overridden (test isolation),
        // keep config under it too rather than falling through to the real
        // user's config directory; otherwise use the OS config-dir convention.
        let config_dir =
            if let Some(dir) = env(&format!("{}_CONFIG_DIR", product.environment_prefix())) {
                dir
            } else if data_dir_override.is_some() {
                data_dir.join("config")
            } else {
                directories::ProjectDirs::from("", "", product.namespace())
                    .ok_or(DiscoveryError::NoHomeDirectory)?
                    .config_dir()
                    .to_path_buf()
            };

        let paths = Self::with_socket(data_dir, socket_path, config_dir);
        paths.validate_socket_path()?;
        Ok(paths)
    }

    /// Derive every runtime path from an explicit data directory (tests and
    /// embedded use). The socket lives under `<data_dir>/run/`. `config_dir`
    /// honors `CODYPENDENT_CONFIG_DIR` if set, otherwise defaults under the
    /// given data dir (never the real user's config directory) — the same
    /// test-isolation rule `resolve` applies when the data dir is overridden.
    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        Self::from_data_dir_for(data_dir, RuntimeProduct::Codypendent)
    }

    /// Embedded-host variant of [`Self::from_data_dir`]. Only the selected
    /// host's explicit configuration override is considered.
    pub fn from_data_dir_for(data_dir: PathBuf, product: RuntimeProduct) -> Self {
        Self::from_data_dir_with_env(data_dir, product, non_empty_env)
    }

    fn from_data_dir_with_env(
        data_dir: PathBuf,
        product: RuntimeProduct,
        env: impl Fn(&str) -> Option<PathBuf>,
    ) -> Self {
        let socket_path = data_dir.join("run").join("daemon.sock");
        let config_dir = env(&format!("{}_CONFIG_DIR", product.environment_prefix()))
            .unwrap_or_else(|| data_dir.join("config"));
        Self::with_socket(data_dir, socket_path, config_dir)
    }

    fn with_socket(data_dir: PathBuf, socket_path: PathBuf, config_dir: PathBuf) -> Self {
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
            config_dir,
        }
    }

    /// Location of the global (trusted) policy overlay: `<config_dir>/policy.toml`.
    ///
    /// Only resolves the path — it does not read or validate the file. A
    /// missing file is a normal, unconfigured state; the caller (`PolicyEngine::load`)
    /// decides how to handle that.
    pub fn global_policy_path(&self) -> PathBuf {
        self.config_dir.join("policy.toml")
    }

    /// Location of the operator-declared MCP server list: `<config_dir>/mcp.toml`.
    ///
    /// Only resolves the path — it does not read or validate the file. A
    /// missing file is a normal, unconfigured state; the caller
    /// (`load_mcp_config`) treats it as an empty config.
    pub fn global_mcp_path(&self) -> PathBuf {
        self.config_dir.join("mcp.toml")
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
    ///
    /// The 0o700 is applied ONLY to a directory this process created, or to one
    /// inside the data dir — never to a pre-existing directory borrowed from
    /// elsewhere. `run_dir` is `socket_path.parent()`, so `CODYPENDENT_SOCKET`
    /// points it anywhere the user likes, and [`DiscoveryError::SocketPathTooLong`]
    /// recommends "for example under /tmp". Chmodding unconditionally took that
    /// advice literally and made the machine's shared `/tmp` private to one
    /// user, breaking every other program on the host.
    pub fn ensure_directories(&self) -> std::io::Result<()> {
        for dir in [&self.data_dir, &self.run_dir, &self.log_dir] {
            let pre_existing = dir.exists();
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Ours to tighten: we just made it, or it lives in our own tree
                // (where a loose mode from an older version is worth fixing).
                // Ours to tighten: we just made it, or it lives in our own tree
                // (where a loose mode from an older version is worth fixing).
                if !pre_existing || dir.starts_with(&self.data_dir) {
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_env(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<PathBuf> {
        let values = values
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        move |key| {
            values
                .get(key)
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
        }
    }

    #[test]
    fn config_dir_env_override_precedence() {
        let product = RuntimeProduct::Codypendent;
        let data = PathBuf::from("/tmp/cp-pf3-data");
        let paths = RuntimePaths::from_data_dir_with_env(data.clone(), product, test_env(&[]));
        assert_eq!(paths.global_policy_path(), data.join("config/policy.toml"));
        assert_eq!(paths.global_mcp_path(), data.join("config/mcp.toml"));

        let paths = RuntimePaths::from_data_dir_with_env(
            data,
            product,
            test_env(&[("CODYPENDENT_CONFIG_DIR", "/tmp/cp-pf3-config")]),
        );
        assert_eq!(paths.config_dir, PathBuf::from("/tmp/cp-pf3-config"));

        let paths = RuntimePaths::resolve_with_env(
            product,
            test_env(&[("CODYPENDENT_DATA_DIR", "/tmp/cp-pf3-data")]),
        )
        .expect("resolves");
        assert_eq!(paths.config_dir, PathBuf::from("/tmp/cp-pf3-data/config"));
        assert_eq!(
            paths.socket_path,
            PathBuf::from("/tmp/cp-pf3-data/run/daemon.sock")
        );

        let paths = RuntimePaths::resolve_with_env(
            product,
            test_env(&[
                ("CODYPENDENT_DATA_DIR", "/tmp/cp-pf3-data"),
                ("CODYPENDENT_CONFIG_DIR", "/tmp/cp-pf3-config"),
                ("CODYPENDENT_SOCKET", "/tmp/cp-pf3-explicit.sock"),
            ]),
        )
        .expect("resolves");
        assert_eq!(paths.config_dir, PathBuf::from("/tmp/cp-pf3-config"));
        assert_eq!(
            paths.socket_path,
            PathBuf::from("/tmp/cp-pf3-explicit.sock")
        );
    }

    #[test]
    fn runtime_product_selection_is_explicit_and_fail_closed() {
        for value in [None, Some(""), Some("   "), Some("codypendent")] {
            assert_eq!(
                RuntimeProduct::from_selector(value).unwrap(),
                RuntimeProduct::Codypendent
            );
        }
        assert_eq!(
            RuntimeProduct::from_selector(Some(" son-of-anton ")).unwrap(),
            RuntimeProduct::SonOfAnton
        );
        assert!(matches!(
            RuntimeProduct::from_selector(Some("unknown-host")),
            Err(DiscoveryError::UnknownRuntimeProduct)
        ));
    }

    #[test]
    fn private_products_ignore_each_others_overrides_and_selector() {
        for (product, prefix, other) in [
            (RuntimeProduct::Codypendent, "CODYPENDENT", "SON_OF_ANTON"),
            (RuntimeProduct::SonOfAnton, "SON_OF_ANTON", "CODYPENDENT"),
        ] {
            let foreign_data = format!("{other}_DATA_DIR");
            let foreign_config = format!("{other}_CONFIG_DIR");
            let foreign_socket = format!("{other}_SOCKET");
            let own_data = format!("{prefix}_DATA_DIR");
            let defaults = RuntimePaths::resolve_with_env(
                product,
                test_env(&[
                    (&foreign_data, "/tmp/foreign-data"),
                    (&foreign_config, "/tmp/foreign-config"),
                    (&foreign_socket, "/tmp/foreign.sock"),
                    ("AGENT_RUNTIME_PRODUCT", "invalid-selector"),
                    ("XDG_RUNTIME_DIR", "/tmp/runtime"),
                ]),
            )
            .expect("explicit profile ignores the selector");
            let platform_dirs =
                directories::ProjectDirs::from("", "", product.namespace()).unwrap();
            assert_eq!(defaults.data_dir, platform_dirs.data_dir());
            assert_eq!(defaults.config_dir, platform_dirs.config_dir());
            assert_eq!(
                defaults.socket_path,
                PathBuf::from("/tmp/runtime")
                    .join(product.namespace())
                    .join("daemon.sock")
            );
            assert_eq!(defaults.log_dir, defaults.data_dir.join("logs"));

            let paths = RuntimePaths::resolve_with_env(
                product,
                test_env(&[
                    (&own_data, "/tmp/private-data"),
                    (&foreign_config, "/tmp/foreign-config"),
                    (&foreign_socket, "/tmp/foreign.sock"),
                    ("XDG_RUNTIME_DIR", "/tmp/runtime"),
                ]),
            )
            .expect("own override resolves");
            assert_eq!(paths.config_dir, PathBuf::from("/tmp/private-data/config"));
            assert_eq!(
                paths.socket_path,
                PathBuf::from("/tmp/private-data/run/daemon.sock")
            );
            assert_eq!(
                paths.pid_path,
                PathBuf::from("/tmp/private-data/run/daemon.pid")
            );
            assert_eq!(paths.log_dir, PathBuf::from("/tmp/private-data/logs"));
        }
    }

    #[test]
    fn anton_overrides_and_embedded_config_remain_private() {
        let env = test_env(&[
            ("SON_OF_ANTON_DATA_DIR", "/tmp/anton-data"),
            ("SON_OF_ANTON_CONFIG_DIR", "/tmp/anton-config"),
            ("SON_OF_ANTON_SOCKET", "/tmp/anton.sock"),
            ("CODYPENDENT_CONFIG_DIR", "/tmp/cody-config"),
        ]);
        let paths = RuntimePaths::resolve_with_env(RuntimeProduct::SonOfAnton, &env).unwrap();
        assert_eq!(paths.data_dir, PathBuf::from("/tmp/anton-data"));
        assert_eq!(paths.config_dir, PathBuf::from("/tmp/anton-config"));
        assert_eq!(paths.socket_path, PathBuf::from("/tmp/anton.sock"));
        let paths = RuntimePaths::from_data_dir_with_env(
            PathBuf::from("/tmp/embedded"),
            RuntimeProduct::SonOfAnton,
            env,
        );
        assert_eq!(paths.config_dir, PathBuf::from("/tmp/anton-config"));
        assert_eq!(
            paths.socket_path,
            PathBuf::from("/tmp/embedded/run/daemon.sock")
        );
    }

    #[test]
    fn blank_overrides_do_not_collapse_a_private_runtime_to_relative_paths() {
        let paths = RuntimePaths::resolve_with_env(
            RuntimeProduct::SonOfAnton,
            test_env(&[
                ("SON_OF_ANTON_DATA_DIR", "  "),
                ("SON_OF_ANTON_CONFIG_DIR", ""),
                ("SON_OF_ANTON_SOCKET", " "),
            ]),
        )
        .unwrap();
        let dirs = directories::ProjectDirs::from("", "", "son-of-anton").unwrap();
        assert_eq!(paths.data_dir, dirs.data_dir());
        assert_eq!(paths.config_dir, dirs.config_dir());
        assert_eq!(paths.socket_path, dirs.data_dir().join("run/daemon.sock"));
    }

    /// `run_dir` is `socket_path.parent()`, and `SocketPathTooLong` tells the
    /// user to put the socket "for example under /tmp". Tightening that borrowed
    /// directory to 0o700 made the machine's shared `/tmp` private to one user.
    /// A directory we did not create and do not own stays exactly as we found it.
    #[cfg(unix)]
    #[test]
    fn a_borrowed_socket_directory_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let shared = PathBuf::from("/tmp/cp-pf3-borrowed-socket-dir");
        let data = PathBuf::from("/tmp/cp-pf3-borrowed-socket-data");
        let _ = std::fs::remove_dir_all(&shared);
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&shared).expect("create the shared dir");
        // 1777: world-writable + sticky, exactly like a real /tmp.
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777))
            .expect("set the shared mode");

        let paths = RuntimePaths::with_socket(
            data.clone(),
            shared.join("daemon.sock"),
            data.join("config"),
        );
        paths.ensure_directories().expect("ensure directories");

        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o7777;
        assert_eq!(
            mode(&shared),
            0o1777,
            "a pre-existing socket directory outside the data dir must not be chmodded"
        );
        // The dirs we do own are still locked down — the socket grants daemon access.
        assert_eq!(mode(&data), 0o700, "the data dir is ours to restrict");
        assert_eq!(
            mode(&paths.log_dir),
            0o700,
            "the log dir is ours to restrict"
        );

        let _ = std::fs::remove_dir_all(&shared);
        let _ = std::fs::remove_dir_all(&data);
    }
}
