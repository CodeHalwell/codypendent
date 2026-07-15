# Phase 0 — Workspace Bootstrap

> **Objective:** an empty directory becomes a Cargo workspace with the Codypendent Protocol crate, a persistent daemon skeleton with SQLite (WAL) and an event-ledger seed, CLI lifecycle commands, fixtures, tests, and CI.
>
> **Specification chapters:** [Roadmap Phase 0](../15-roadmap.md), [System Architecture](../02-system-architecture.md), [Daemon and Client Protocol](../03-daemon-client-protocol.md), [Core Data Contracts](../14-core-data-contracts.md), [Testing Strategy](../16-testing-strategy.md).
>
> **Exit criteria (from the roadmap):** `codypendent daemon start`, `codypendent daemon status --json`, and `codypendent daemon stop` work; daemon restart preserves its instance database; a fixture event log replays deterministically.

Every file in this chapter is literal and **verified**: the exact contents below were compiled with `cargo build`, linted with `cargo clippy --all-targets --all-features -- -D warnings`, formatted with `cargo fmt --check`, and exercised by the tests and end-to-end commands shown in the checkpoints, before this guide was published. Copy them exactly (guide rule 3).

What you build here and why it is shaped this way:

- **Four crates, not nine.** The manual's target layout lists nine module directories; CONTRIBUTING forbids creating crates that merely mirror diagrams. Phase 0 needs exactly: `protocol` (wire types, IDs, framing, discovery), `daemon` (persistence + server, binary `codypendentd`), `cli` (binary `codypendent`), `test-support` (fixtures). Later phases add `runtime`, `tui`, `knowledge`, `integrations`, `sandbox` when they earn existence.
- **The event ledger exists from day one.** Sessions and an append-only `events` table with `(session_id, sequence)` as primary key are created in the first migration, because every later subsystem (runs, approvals, projections, recovery) builds on this ordering authority.
- **`agent-framework-core = "0.1.1"` is pinned now** in workspace dependencies (a roadmap Phase 0 deliverable) and first consumed in Phase 1.
- **Socket discovery is protocol.** Clients and daemon must independently resolve the same socket path; Unix socket paths are limited to ~104–108 bytes, so resolution prefers `$XDG_RUNTIME_DIR` and validates length with a structured error instead of an opaque `SUN_LEN` bind failure.

## STEP 0.1 — Verify the environment

**RUN**

```bash
rustc --version && cargo --version && git --version
```

**EXPECT** — `rustc` ≥ 1.82 stable, any recent `cargo`, `git` ≥ 2.40. If `rustc` is missing or too old, install/update via rustup (see the [guide overview](00-how-to-use-this-guide.md)) before continuing.

## STEP 0.2 — Create the repository skeleton

Work in the directory that is (or will become) the `codypendent` product repository root. If the repository already exists and contains only documentation (`README.md`, `docs/`), keep those files and add the code alongside them — nothing in this phase conflicts with them.

**RUN**

```bash
git init 2>/dev/null || true
mkdir -p crates/protocol/src crates/daemon/src crates/daemon/tests \
         crates/cli/src crates/test-support/src crates/test-support/fixtures \
         migrations .github/workflows
```

## STEP 0.3 — Root workspace files

Three files at the repository root. The workspace pins shared dependency versions once; member crates reference them with `workspace = true`. Lints are strict from the first commit: warnings are errors in CI, `unsafe_code` is denied.

**CREATE FILE `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = [
    "crates/protocol",
    "crates/daemon",
    "crates/cli",
    "crates/test-support",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.82"
repository = "https://github.com/CodeHalwell/codypendent"

[workspace.dependencies]
# Internal crates
codypendent-protocol = { path = "crates/protocol" }
codypendent-daemon = { path = "crates/daemon" }
codypendent-test-support = { path = "crates/test-support" }

# agent-framework-rs is pinned here in Phase 0 (roadmap requirement) and first
# consumed by the runtime layer in Phase 1.
agent-framework-core = "0.1.1"

# Async runtime and serialization
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "signal", "time", "process", "fs", "sync"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Identifiers and time
uuid = { version = "1", features = ["v7", "serde"] }
chrono = { version = "0.4", features = ["serde"] }

# Errors and logging
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Storage
sqlx = { version = "0.8", default-features = false, features = ["runtime-tokio", "sqlite", "migrate", "macros", "chrono", "uuid"] }

# CLI and paths
clap = { version = "4", features = ["derive"] }
directories = "6"

# Test utilities
tempfile = "3"

[workspace.lints.rust]
unsafe_code = "deny"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

**CREATE FILE `rust-toolchain.toml`**

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

**CREATE FILE `.gitignore`**

```text
/target
/book
```

Note on `.gitignore`: if the repository already has one, append the two lines instead of overwriting.

**CHECKPOINT**

```bash
cargo metadata --format-version 1 > /dev/null
```

This fails right now with "failed to read …/crates/protocol/Cargo.toml" — that is the **expected** state (members don't exist yet) and confirms the workspace file is being parsed. Continue.

## STEP 0.4 — The first migration

Migrations live at the repository root in `migrations/` and are embedded into the daemon binary at compile time (`sqlx::migrate!`). Rule for all phases: **migrations are append-only** — a committed migration file is never edited; schema changes are new numbered files.

**CREATE FILE `migrations/0001_init.sql`**

```sql
-- Phase 0 authoritative store.
-- SQLite in WAL mode is the local metadata and event authority (ADR-003).
-- Every later phase adds tables through new numbered migrations; existing
-- migrations are never edited after they have been committed.

CREATE TABLE daemon_instance (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    instance_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    boot_count INTEGER NOT NULL DEFAULT 0,
    last_started_at TEXT
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT,
    title TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0
);

-- The append-only event ledger. `sequence` is monotonic per session and the
-- pair (session_id, sequence) is the durable ordering authority (invariant 5).
CREATE TABLE events (
    session_id TEXT NOT NULL REFERENCES sessions(id),
    sequence INTEGER NOT NULL,
    occurred_at TEXT NOT NULL,
    actor TEXT NOT NULL,
    body TEXT NOT NULL,
    causation_id TEXT,
    correlation_id TEXT,
    schema_version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (session_id, sequence)
);
```

## STEP 0.5 — The protocol crate

`codypendent-protocol` owns wire types, identifiers, envelopes, framing, and daemon discovery ([Chapter 03](../03-daemon-client-protocol.md), [Chapter 14](../14-core-data-contracts.md)). Eight files. Behavioural notes you must preserve if you ever touch this code:

- IDs are UUIDv7 newtypes (sortable, `#[serde(transparent)]`); `ModelId` and `UserId` are strings by design.
- The envelope carries `protocol_version`; the server rejects incompatible **major** versions with a structured `Error` payload, never by closing the connection silently.
- Framing is `u32` **big-endian** length prefix + JSON, 16 MiB cap; `read_envelope` returns `Ok(None)` on clean EOF so connection loops terminate without error noise.
- Discovery resolves the socket as: `CODYPENDENT_SOCKET` override → under `CODYPENDENT_DATA_DIR` if set → `$XDG_RUNTIME_DIR/codypendent/` → `<data>/run/`; and validates the ~104-byte Unix limit up front.

**CREATE FILE `crates/protocol/Cargo.toml`**

```toml
[package]
name = "codypendent-protocol"
description = "Codypendent Protocol: wire types, identifiers, envelopes, framing, and daemon discovery."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
uuid = { workspace = true }
chrono = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
directories = { workspace = true }

[lints]
workspace = true
```

**CREATE FILE `crates/protocol/src/lib.rs`**

```rust
//! Codypendent Protocol.
//!
//! Wire types, identifiers, envelopes, framing, and daemon discovery shared by
//! `codypendentd` and every client (CLI, TUI, IDE bridges, headless).
//!
//! Rules that hold for the whole protocol crate:
//! - types here are serialization contracts; behaviour lives in the daemon;
//! - fields are additive by default; breaking changes require a new major
//!   protocol version;
//! - unknown enum variants must be handled safely by receivers.

pub mod discovery;
pub mod envelope;
pub mod events;
pub mod framing;
pub mod ids;
pub mod version;

pub use envelope::{DaemonStatus, Envelope, Payload, ProtocolError};
pub use events::{Actor, EventBody, SessionEvent};
pub use framing::{read_envelope, write_envelope, FrameError, MAX_FRAME_BYTES};
pub use ids::*;
pub use version::{ProtocolVersion, PROTOCOL_V1};
```

**CREATE FILE `crates/protocol/src/ids.rs`**

```rust
//! Opaque, sortable identifiers (UUIDv7) for every domain entity.
//!
//! See "Core Data Contracts". IDs are newtypes so they can never be confused
//! with one another at compile time.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Create a new time-ordered (UUIDv7) identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

uuid_id!(SessionId);
uuid_id!(RunId);
uuid_id!(TaskId);
uuid_id!(AgentId);
uuid_id!(ArtifactId);
uuid_id!(WorkflowId);
uuid_id!(ToolId);
uuid_id!(SkillId);
uuid_id!(PluginId);
uuid_id!(DocumentId);
uuid_id!(WorkspaceId);
uuid_id!(ClientId);
uuid_id!(MessageId);
uuid_id!(CommandId);
uuid_id!(CorrelationId);
uuid_id!(ApprovalId);
uuid_id!(DaemonInstanceId);

/// Model identifiers are provider strings such as `"claude-sonnet-5"` or
/// `"qwen2.5-coder:32b"`, not UUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// User identifiers are strings in the personal product (OS user or configured
/// identity), not UUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
```

**CREATE FILE `crates/protocol/src/version.rs`**

```rust
//! Protocol version negotiation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

/// The current protocol version. Additive changes bump `minor`; breaking
/// changes bump `major` and require negotiation.
pub const PROTOCOL_V1: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

impl ProtocolVersion {
    /// Two versions are compatible when their major versions match.
    pub fn compatible_with(&self, other: &ProtocolVersion) -> bool {
        self.major == other.major
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}
```

**CREATE FILE `crates/protocol/src/envelope.rs`**

```rust
//! The message envelope and the Phase 0 payload set.
//!
//! Every frame on the wire is one serialized `Envelope`. The payload enum
//! grows in later phases (sessions, runs, subscriptions, approvals, ...);
//! Phase 0 ships only daemon lifecycle messages.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{ClientId, DaemonInstanceId, MessageId, SessionId, WorkspaceId};
use crate::version::{ProtocolVersion, PROTOCOL_V1};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: ProtocolVersion,
    pub message_id: MessageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<MessageId>,
    pub client_id: ClientId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    pub payload: Payload,
}

impl Envelope {
    /// Build a new request envelope from a client.
    pub fn request(client_id: ClientId, payload: Payload) -> Self {
        Self {
            protocol_version: PROTOCOL_V1,
            message_id: MessageId::new(),
            correlation_id: None,
            client_id,
            workspace_id: None,
            session_id: None,
            sequence: None,
            payload,
        }
    }

    /// Build a reply correlated to `request`.
    pub fn reply_to(request: &Envelope, payload: Payload) -> Self {
        Self {
            protocol_version: PROTOCOL_V1,
            message_id: MessageId::new(),
            correlation_id: Some(request.message_id),
            client_id: request.client_id,
            workspace_id: request.workspace_id,
            session_id: request.session_id,
            sequence: None,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Payload {
    /// Liveness probe.
    Ping,
    Pong,
    /// Ask the daemon to describe itself.
    DaemonStatusRequest,
    DaemonStatusResponse(DaemonStatus),
    /// Ask the daemon to shut down gracefully.
    Shutdown,
    ShutdownAck,
    /// Structured protocol-level error (never parse human text to decide
    /// behaviour).
    Error(ProtocolError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub daemon_version: String,
    pub protocol_version: ProtocolVersion,
    pub instance_id: DaemonInstanceId,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub uptime_seconds: u64,
    pub boot_count: i64,
    pub database_path: String,
    pub socket_path: String,
    pub session_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
```

**CREATE FILE `crates/protocol/src/events.rs`**

```rust
//! Durable session events.
//!
//! Events record accepted state changes or observations. They are persisted
//! in the event ledger before any client observes them, and original events
//! are immutable evidence (invariant 5). The `EventBody` set here is the
//! Phase 0 seed; later phases add run, tool, approval, patch, workflow, and
//! document events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, ClientId, CommandId, CorrelationId, ModelId, RunId, UserId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<CommandId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<CorrelationId>,
    pub actor: Actor,
    pub body: EventBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Actor {
    Human {
        user_id: UserId,
    },
    Agent {
        agent_id: AgentId,
        run_id: RunId,
        model: ModelId,
    },
    Client {
        client_id: ClientId,
    },
    Integration {
        integration_id: String,
    },
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum EventBody {
    SessionCreated { title: String },
    NoteAppended { text: String },
    SessionClosed,
}
```

**CREATE FILE `crates/protocol/src/framing.rs`**

```rust
//! Length-prefixed JSON framing.
//!
//! ```text
//! +----------------------+-------------------------+
//! | u32 payload length   | serialized envelope     |
//! +----------------------+-------------------------+
//! ```
//!
//! The length prefix is big-endian. JSON is the first serialization for
//! inspectability; MessagePack may be negotiated in a later protocol version.
//! Large data travels as artifact references, never as huge frames.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::envelope::Envelope;

/// Frames larger than this are a protocol violation (16 MiB).
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds MAX_FRAME_BYTES")]
    TooLarge(usize),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Write one envelope as a length-prefixed frame and flush.
pub async fn write_envelope<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &Envelope,
) -> Result<(), FrameError> {
    let bytes = serde_json::to_vec(envelope)?;
    let len = u32::try_from(bytes.len()).map_err(|_| FrameError::TooLarge(bytes.len()))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one envelope. Returns `Ok(None)` on a clean end-of-stream before the
/// first length byte.
pub async fn read_envelope<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Envelope>, FrameError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len as usize));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}
```

**CREATE FILE `crates/protocol/src/discovery.rs`**

```rust
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
```

**CHECKPOINT**

```bash
cargo check -p codypendent-protocol
```

**EXPECT** — compiles with no warnings.

**COMMIT** `"phase0: workspace root, first migration, protocol crate"`

## STEP 0.6 — The daemon crate

`codypendent-daemon` (binary name `codypendentd`) owns the database, instance identity, the event ledger, projection replay, and the socket server. Eight files. Behavioural notes:

- `open_database` enables WAL + NORMAL synchronous + foreign keys + 5s busy timeout, and runs migrations on every startup.
- `record_boot` proves persistence: the single-row `daemon_instance` table keeps a stable `instance_id` and increments `boot_count` on each boot.
- The ledger's caller supplies `sequence`; the `(session_id, sequence)` primary key makes a duplicate append a loud error, never a silent fork (invariant 4 groundwork).
- `replay::project` must be a pure fold — same events in, same projection out (property tested in the testing strategy).
- The server refuses to start when a live daemon answers on the socket, removes a stale socket file, writes a pidfile, handles Shutdown/SIGTERM/SIGINT gracefully, and removes socket + pidfile on exit.

**CREATE FILE `crates/daemon/Cargo.toml`**

```toml
[package]
name = "codypendent-daemon"
description = "codypendentd: persistence, command handling, subscriptions, and recovery."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
repository.workspace = true

[[bin]]
name = "codypendentd"
path = "src/main.rs"

[dependencies]
codypendent-protocol = { workspace = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
chrono = { workspace = true }
uuid = { workspace = true }
thiserror = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
sqlx = { workspace = true }

[dev-dependencies]
codypendent-test-support = { workspace = true }
tempfile = { workspace = true }

[lints]
workspace = true
```

**CREATE FILE `crates/daemon/src/lib.rs`**

```rust
//! `codypendentd` library: persistence, ledger, replay, and the client
//! protocol server. The binary in `src/main.rs` wires these together.

pub mod db;
pub mod instance;
pub mod ledger;
pub mod replay;
pub mod server;
```

**CREATE FILE `crates/daemon/src/db.rs`**

```rust
//! SQLite (WAL mode) — the authoritative local metadata and event store
//! (ADR-003). Migrations are embedded at compile time from `migrations/` at
//! the repository root and run on every startup.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};

pub async fn open_database(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;

    sqlx::migrate!("../../migrations").run(&pool).await?;
    Ok(pool)
}
```

**CREATE FILE `crates/daemon/src/instance.rs`**

```rust
//! Daemon instance identity.
//!
//! The single-row `daemon_instance` table proves that daemon state survives
//! restarts: the instance ID is created once and `boot_count` increments on
//! every boot.

use chrono::{DateTime, Utc};
use codypendent_protocol::DaemonInstanceId;
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct InstanceRecord {
    pub instance_id: DaemonInstanceId,
    pub created_at: DateTime<Utc>,
    pub boot_count: i64,
}

/// Insert the instance row on first boot; increment `boot_count` on every
/// boot. Returns the current record.
pub async fn record_boot(pool: &SqlitePool) -> anyhow::Result<InstanceRecord> {
    let now = Utc::now();
    let existing: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT instance_id, created_at, boot_count FROM daemon_instance WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;

    match existing {
        Some((instance_id, created_at, boot_count)) => {
            let boot_count = boot_count + 1;
            sqlx::query(
                "UPDATE daemon_instance SET boot_count = ?, last_started_at = ? WHERE id = 1",
            )
            .bind(boot_count)
            .bind(now.to_rfc3339())
            .execute(pool)
            .await?;
            Ok(InstanceRecord {
                instance_id: instance_id.parse()?,
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                boot_count,
            })
        }
        None => {
            let instance_id = DaemonInstanceId::new();
            sqlx::query(
                "INSERT INTO daemon_instance (id, instance_id, created_at, boot_count, last_started_at) \
                 VALUES (1, ?, ?, 1, ?)",
            )
            .bind(instance_id.to_string())
            .bind(now.to_rfc3339())
            .bind(now.to_rfc3339())
            .execute(pool)
            .await?;
            Ok(InstanceRecord {
                instance_id,
                created_at: now,
                boot_count: 1,
            })
        }
    }
}
```

**CREATE FILE `crates/daemon/src/ledger.rs`**

```rust
//! The append-only event ledger.
//!
//! Phase 0 provides create/append/load/count. Later phases add commands with
//! idempotency keys, the crash-consistency write path, projections, and
//! subscriptions — the storage shape here is already the durable ordering
//! authority they build on.

use chrono::{DateTime, Utc};
use codypendent_protocol::{SessionEvent, SessionId};
use sqlx::SqlitePool;

/// Insert a session row in state `open`.
pub async fn create_session(
    pool: &SqlitePool,
    session_id: SessionId,
    title: &str,
) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sessions (id, title, state, created_at, updated_at, revision) \
         VALUES (?, ?, 'open', ?, ?, 0)",
    )
    .bind(session_id.to_string())
    .bind(title)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Append one event. The caller supplies `event.sequence`; the UNIQUE primary
/// key (session_id, sequence) makes duplicate appends fail loudly instead of
/// silently forking history.
pub async fn append_event(
    pool: &SqlitePool,
    session_id: SessionId,
    event: &SessionEvent,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(session_id.to_string())
    .bind(i64::try_from(event.sequence)?)
    .bind(event.occurred_at.to_rfc3339())
    .bind(serde_json::to_string(&event.actor)?)
    .bind(serde_json::to_string(&event.body)?)
    .bind(event.causation_id.map(|id| id.to_string()))
    .bind(event.correlation_id.map(|id| id.to_string()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Row shape of the `events` table used by `load_events`:
/// (sequence, occurred_at, actor, body, causation_id, correlation_id).
type EventRow = (i64, String, String, String, Option<String>, Option<String>);

/// Load every event for a session in sequence order.
pub async fn load_events(
    pool: &SqlitePool,
    session_id: SessionId,
) -> anyhow::Result<Vec<SessionEvent>> {
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT sequence, occurred_at, actor, body, causation_id, correlation_id \
         FROM events WHERE session_id = ? ORDER BY sequence ASC",
    )
    .bind(session_id.to_string())
    .fetch_all(pool)
    .await?;

    let mut events = Vec::with_capacity(rows.len());
    for (sequence, occurred_at, actor, body, causation_id, correlation_id) in rows {
        events.push(SessionEvent {
            sequence: u64::try_from(sequence)?,
            occurred_at: DateTime::parse_from_rfc3339(&occurred_at)?.with_timezone(&Utc),
            causation_id: causation_id.map(|id| id.parse()).transpose()?,
            correlation_id: correlation_id.map(|id| id.parse()).transpose()?,
            actor: serde_json::from_str(&actor)?,
            body: serde_json::from_str(&body)?,
        });
    }
    Ok(events)
}

/// The next sequence number for a session (1-based).
pub async fn next_sequence(pool: &SqlitePool, session_id: SessionId) -> anyhow::Result<u64> {
    let (max,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(pool)
            .await?;
    Ok(u64::try_from(max)? + 1)
}

pub async fn session_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
        .fetch_one(pool)
        .await?;
    Ok(count)
}
```

**CREATE FILE `crates/daemon/src/replay.rs`**

```rust
//! Projection replay.
//!
//! Projections are derived state: folding the same events must always produce
//! the same projection (tested property). This Phase 0 projection is small;
//! later phases add run, approval, and workflow projections built the same
//! way.

use codypendent_protocol::{EventBody, SessionEvent};
use serde::Serialize;

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct SessionProjection {
    pub title: Option<String>,
    pub note_count: u64,
    pub closed: bool,
    pub last_sequence: u64,
    pub event_count: u64,
}

pub fn project(events: &[SessionEvent]) -> SessionProjection {
    let mut projection = SessionProjection::default();
    for event in events {
        projection.event_count += 1;
        projection.last_sequence = event.sequence;
        match &event.body {
            EventBody::SessionCreated { title } => projection.title = Some(title.clone()),
            EventBody::NoteAppended { .. } => projection.note_count += 1,
            EventBody::SessionClosed => projection.closed = true,
            _ => {}
        }
    }
    projection
}
```

**CREATE FILE `crates/daemon/src/server.rs`**

```rust
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
```

**CREATE FILE `crates/daemon/src/main.rs`**

```rust
//! `codypendentd` — the persistent Codypendent daemon.

use codypendent_daemon::{db, instance, server};
use codypendent_protocol::discovery::RuntimePaths;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let paths = RuntimePaths::resolve()?;
    paths.ensure_directories()?;
    let database_path = paths.data_dir.join("codypendent.db");

    let pool = db::open_database(&database_path).await?;
    let boot = instance::record_boot(&pool).await?;
    info!(
        instance = %boot.instance_id,
        boot_count = boot.boot_count,
        database = %database_path.display(),
        "codypendentd starting"
    );

    server::run(pool, paths, boot).await
}
```

**CHECKPOINT**

```bash
cargo check -p codypendent-daemon
```

**EXPECT** — compiles with no warnings.

**COMMIT** `"phase0: daemon crate with ledger, replay, and socket server"`

## STEP 0.7 — The CLI crate

`codypendent-cli` (binary name `codypendent`) implements the Phase 0 surface: `daemon start|stop|status [--json]`. Behavioural notes:

- `start` is idempotent: if a daemon already answers Ping, it reports and exits 0. Otherwise it spawns `codypendentd` (preferring the binary next to its own executable, falling back to `PATH`), detached into its own process group with output redirected to `<data>/logs/daemon.log`, and polls Ping for up to 5 seconds.
- `stop` is idempotent the same way, and confirms the socket actually stops answering.
- `status --json` prints `{"running": true, "status": {…}}` or `{"running": false}`; exit code 0 when running, 1 when not. Machine-readable output is a contract — scripts parse it (never make them parse human text).

**CREATE FILE `crates/cli/Cargo.toml`**

```toml
[package]
name = "codypendent-cli"
description = "codypendent: the command-line entry point (daemon lifecycle in Phase 0; TUI attaches in Phase 1)."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
repository.workspace = true

[[bin]]
name = "codypendent"
path = "src/main.rs"

[dependencies]
codypendent-protocol = { workspace = true }
tokio = { workspace = true }
clap = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }

[lints]
workspace = true
```

**CREATE FILE `crates/cli/src/main.rs`**

```rust
//! `codypendent` — CLI entry point.
//!
//! Phase 0 surface:
//!
//! ```text
//! codypendent daemon start
//! codypendent daemon status [--json]
//! codypendent daemon stop
//! ```

mod client;
mod commands;

use clap::{Parser, Subcommand};
use codypendent_protocol::discovery::RuntimePaths;

#[derive(Parser)]
#[command(
    name = "codypendent",
    version,
    about = "Codypendent — the local-first agentic developer environment"
)]
struct Cli {
    #[command(subcommand)]
    command: TopCommand,
}

#[derive(Subcommand)]
enum TopCommand {
    /// Manage the codypendentd daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Start the daemon if it is not already running.
    Start,
    /// Ask a running daemon to shut down gracefully.
    Stop,
    /// Show daemon status. Exit code 0 when running, 1 when not.
    Status {
        /// Print machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let paths = RuntimePaths::resolve()?;
    match cli.command {
        TopCommand::Daemon { command } => match command {
            DaemonCommand::Start => commands::start(&paths).await,
            DaemonCommand::Stop => commands::stop(&paths).await,
            DaemonCommand::Status { json } => commands::status(&paths, json).await,
        },
    }
}
```

**CREATE FILE `crates/cli/src/client.rs`**

```rust
//! Minimal protocol client used by CLI commands.

use std::path::Path;

use codypendent_protocol::{
    read_envelope, write_envelope, ClientId, DaemonStatus, Envelope, Payload,
};
use tokio::net::UnixStream;

async fn request(socket: &Path, payload: Payload) -> anyhow::Result<Envelope> {
    let mut stream = UnixStream::connect(socket).await?;
    let request = Envelope::request(ClientId::new(), payload);
    write_envelope(&mut stream, &request).await?;
    match read_envelope(&mut stream).await? {
        Some(reply) => Ok(reply),
        None => anyhow::bail!("daemon closed the connection before replying"),
    }
}

/// True when a daemon answers Ping with Pong on this socket.
pub async fn ping(socket: &Path) -> bool {
    matches!(
        request(socket, Payload::Ping).await,
        Ok(Envelope {
            payload: Payload::Pong,
            ..
        })
    )
}

pub async fn daemon_status(socket: &Path) -> anyhow::Result<DaemonStatus> {
    match request(socket, Payload::DaemonStatusRequest).await?.payload {
        Payload::DaemonStatusResponse(status) => Ok(status),
        other => anyhow::bail!("unexpected reply to status request: {other:?}"),
    }
}

pub async fn shutdown(socket: &Path) -> anyhow::Result<()> {
    match request(socket, Payload::Shutdown).await?.payload {
        Payload::ShutdownAck => Ok(()),
        other => anyhow::bail!("unexpected reply to shutdown request: {other:?}"),
    }
}
```

**CREATE FILE `crates/cli/src/commands.rs`**

```rust
//! Daemon lifecycle commands.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use codypendent_protocol::discovery::RuntimePaths;

use crate::client;

/// `codypendent daemon start`: spawn `codypendentd` detached, then wait for
/// the socket to answer Ping (5 second budget).
pub async fn start(paths: &RuntimePaths) -> anyhow::Result<()> {
    if client::ping(&paths.socket_path).await {
        println!("daemon already running");
        return Ok(());
    }
    paths.ensure_directories()?;

    let daemon_binary = resolve_daemon_binary();
    let log_path = paths.log_dir.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_for_stderr = log.try_clone()?;

    let mut command = std::process::Command::new(&daemon_binary);
    command
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_for_stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group: the daemon must not die with this CLI's terminal.
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", daemon_binary.display()))?;

    for _ in 0..50 {
        if client::ping(&paths.socket_path).await {
            println!("daemon started (pid {})", child.id());
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "daemon did not become ready within 5 seconds; check {}",
        log_path.display()
    )
}

/// `codypendent daemon stop`: request graceful shutdown, then wait for the
/// socket to stop answering (5 second budget).
pub async fn stop(paths: &RuntimePaths) -> anyhow::Result<()> {
    if !client::ping(&paths.socket_path).await {
        println!("daemon is not running");
        return Ok(());
    }
    client::shutdown(&paths.socket_path).await?;
    for _ in 0..50 {
        if !client::ping(&paths.socket_path).await {
            println!("daemon stopped");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("daemon acknowledged shutdown but is still answering after 5 seconds")
}

/// `codypendent daemon status [--json]`.
pub async fn status(paths: &RuntimePaths, json: bool) -> anyhow::Result<()> {
    match client::daemon_status(&paths.socket_path).await {
        Ok(status) => {
            if json {
                let value = serde_json::json!({ "running": true, "status": status });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Codypendent daemon");
                println!("  running      yes");
                println!("  version      {}", status.daemon_version);
                println!("  protocol     {}", status.protocol_version);
                println!("  pid          {}", status.pid);
                println!("  instance     {}", status.instance_id);
                println!("  boot count   {}", status.boot_count);
                println!("  started at   {}", status.started_at.to_rfc3339());
                println!("  uptime       {}s", status.uptime_seconds);
                println!("  database     {}", status.database_path);
                println!("  socket       {}", status.socket_path);
                println!("  sessions     {}", status.session_count);
            }
            Ok(())
        }
        Err(_) => {
            if json {
                println!("{}", serde_json::json!({ "running": false }));
            } else {
                println!("daemon is not running");
            }
            std::process::exit(1);
        }
    }
}

/// Prefer a `codypendentd` sitting next to this executable (the layout that
/// `cargo build` and installers both produce); fall back to PATH lookup.
fn resolve_daemon_binary() -> PathBuf {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let candidate = dir.join("codypendentd");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("codypendentd")
}
```

**CHECKPOINT**

```bash
cargo check -p codypendent-cli
```

**EXPECT** — compiles with no warnings.

## STEP 0.8 — Test support and the event fixture

The fixture is stored as **raw JSONL, one serialized `SessionEvent` per line**, so future protocol-compatibility tests replay the exact historical bytes (Chapter 16: fixture corpora for previous versions). Do not regenerate it from structs.

**CREATE FILE `crates/test-support/Cargo.toml`**

```toml
[package]
name = "codypendent-test-support"
description = "Shared fixtures and helpers for Codypendent tests."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
codypendent-protocol = { workspace = true }
serde_json = { workspace = true }

[lints]
workspace = true
```

**CREATE FILE `crates/test-support/src/lib.rs`**

```rust
//! Shared fixtures and helpers for Codypendent tests.

use codypendent_protocol::SessionEvent;

/// Raw JSONL of the basic event fixture (one serialized `SessionEvent` per
/// line). Kept as text so protocol-compatibility tests can replay the exact
/// historical bytes, not merely re-serialized structs.
pub fn fixture_events_jsonl() -> &'static str {
    include_str!("../fixtures/events-basic.jsonl")
}

/// Parse the basic fixture into events. Panics on malformed fixture content —
/// a broken fixture is a bug in the repository, not a runtime condition.
pub fn load_fixture_events() -> Vec<SessionEvent> {
    fixture_events_jsonl()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("fixture line must parse as SessionEvent"))
        .collect()
}
```

**CREATE FILE `crates/test-support/fixtures/events-basic.jsonl`**

```text
{"sequence":1,"occurred_at":"2026-07-14T09:00:00Z","actor":{"type":"System"},"body":{"type":"SessionCreated","title":"fixture session"}}
{"sequence":2,"occurred_at":"2026-07-14T09:00:05Z","actor":{"type":"Human","user_id":"dana"},"body":{"type":"NoteAppended","text":"first note"}}
{"sequence":3,"occurred_at":"2026-07-14T09:00:10Z","actor":{"type":"Human","user_id":"dana"},"body":{"type":"NoteAppended","text":"second note"}}
{"sequence":4,"occurred_at":"2026-07-14T09:00:15Z","actor":{"type":"System"},"body":{"type":"SessionClosed"}}
```

## STEP 0.9 — Integration tests

Three test files under `crates/daemon/tests/`, covering the roadmap's Phase 0 exit criteria plus two testing-strategy properties (replay determinism; duplicate-delivery rejection) and protocol behaviour over a real socket (ping, status, version rejection, graceful shutdown with cleanup).

**CREATE FILE `crates/daemon/tests/persistence.rs`**

```rust
//! Phase 0 exit criterion: daemon restart preserves its instance database.

use codypendent_daemon::{db, instance};

#[tokio::test]
async fn instance_identity_survives_restart() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let db_path = tmp.path().join("codypendent.db");

    let pool1 = db::open_database(&db_path)
        .await
        .expect("open db first time");
    let boot1 = instance::record_boot(&pool1).await.expect("first boot");
    assert_eq!(boot1.boot_count, 1);
    pool1.close().await;

    let pool2 = db::open_database(&db_path)
        .await
        .expect("open db second time");
    let boot2 = instance::record_boot(&pool2).await.expect("second boot");

    assert_eq!(
        boot2.instance_id, boot1.instance_id,
        "instance identity must persist"
    );
    assert_eq!(
        boot2.boot_count, 2,
        "boot count must increment across restarts"
    );
}
```

**CREATE FILE `crates/daemon/tests/replay.rs`**

```rust
//! Phase 0 exit criterion: the daemon can replay a fixture event log, and
//! replay through the ledger produces the same projection as folding the
//! fixture directly (event replay determinism, from the testing strategy).

use codypendent_daemon::{db, ledger, replay};
use codypendent_protocol::SessionId;

#[tokio::test]
async fn fixture_replay_produces_expected_projection() {
    let events = codypendent_test_support::load_fixture_events();
    let direct = replay::project(&events);
    assert_eq!(direct.title.as_deref(), Some("fixture session"));
    assert_eq!(direct.note_count, 2);
    assert!(direct.closed);
    assert_eq!(direct.last_sequence, 4);
    assert_eq!(direct.event_count, 4);

    let tmp = tempfile::tempdir().expect("create temp dir");
    let pool = db::open_database(&tmp.path().join("codypendent.db"))
        .await
        .expect("open db");
    let session_id = SessionId::new();
    ledger::create_session(&pool, session_id, "fixture session")
        .await
        .expect("create session");
    for event in &events {
        ledger::append_event(&pool, session_id, event)
            .await
            .expect("append event");
    }

    let loaded = ledger::load_events(&pool, session_id)
        .await
        .expect("load events");
    assert_eq!(
        loaded, events,
        "ledger round-trip must preserve events exactly"
    );
    assert_eq!(
        replay::project(&loaded),
        direct,
        "replay must be deterministic"
    );

    let next = ledger::next_sequence(&pool, session_id)
        .await
        .expect("next sequence");
    assert_eq!(next, 5);
}

#[tokio::test]
async fn duplicate_sequence_is_rejected() {
    let events = codypendent_test_support::load_fixture_events();
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pool = db::open_database(&tmp.path().join("codypendent.db"))
        .await
        .expect("open db");
    let session_id = SessionId::new();
    ledger::create_session(&pool, session_id, "fixture session")
        .await
        .expect("create session");
    ledger::append_event(&pool, session_id, &events[0])
        .await
        .expect("first append succeeds");
    let duplicate = ledger::append_event(&pool, session_id, &events[0]).await;
    assert!(
        duplicate.is_err(),
        "appending the same sequence twice must fail"
    );
}
```

**CREATE FILE `crates/daemon/tests/socket.rs`**

```rust
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
```

## STEP 0.10 — Continuous integration

The CONTRIBUTING gates (`fmt`, `clippy -D warnings`, `test`) run on every push and pull request from the first commit.

**CREATE FILE `.github/workflows/ci.yml`**

```yaml
name: ci

on:
  push:
    branches: [main]
  pull_request:

env:
  CARGO_TERM_COLOR: always

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - name: Format
        run: cargo fmt --all -- --check
      - name: Lint
        run: cargo clippy --workspace --all-targets --all-features -- -D warnings
      - name: Test
        run: cargo test --workspace
```

## STEP 0.11 — Full verification

**CHECKPOINT**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

**EXPECT** — all three succeed. The test run must show these tests passing:

```text
instance_identity_survives_restart ... ok
duplicate_sequence_is_rejected ... ok
fixture_replay_produces_expected_projection ... ok
ping_status_shutdown_over_socket ... ok
```

**COMMIT** `"phase0: cli, test support, fixtures, integration tests, ci"`

## STEP 0.12 — End-to-end exit criteria

Run the roadmap's exit-criteria commands against the real binaries. Use an isolated data directory so this drill never touches real user state. **The data directory path must be short** (Unix socket limit — see Step 0.5 notes); `/tmp/cody-e2e` is safe.

**RUN**

```bash
cargo build --workspace
export CODYPENDENT_DATA_DIR=/tmp/cody-e2e
rm -rf /tmp/cody-e2e
./target/debug/codypendent daemon start
./target/debug/codypendent daemon status --json
./target/debug/codypendent daemon stop
```

**EXPECT**

```text
daemon started (pid <PID>)
{
  "running": true,
  "status": {
    "boot_count": 1,
    "daemon_version": "0.1.0",
    "database_path": "/tmp/cody-e2e/codypendent.db",
    "instance_id": "<uuid>",
    ...
  }
}
daemon stopped
```

**RUN** — restart persistence check:

```bash
./target/debug/codypendent daemon start
./target/debug/codypendent daemon status --json | grep -E '"boot_count"|"instance_id"'
./target/debug/codypendent daemon stop
./target/debug/codypendent daemon status --json; echo "exit=$?"
unset CODYPENDENT_DATA_DIR
rm -rf /tmp/cody-e2e
```

**EXPECT** — `boot_count` is `2`, `instance_id` is **identical** to the first run (restart preserved the instance database), and the final status prints `{"running":false}` with `exit=1`.

**COMMIT** — nothing to commit in this step (verification only). If the working tree is dirty here, you deviated somewhere: stop and reconcile.

## Identity and go-to-market overlay (no code)

`TIMELINE.md` Phase 0–1 items ride alongside this engineering phase and are for the human owner, not the build agent: confirm the `Codypendent` name, trademark screening (UK/US), reserve domains/org handles, check `crates.io`/npm namespaces, publish the naming guide (`docs/product/`). Naming guardrails **are** enforced in code: binaries are exactly `codypendent` and `codypendentd`; never introduce a `cody` executable (Sourcegraph Cody overlap).

## Deferred item registry (revisit in later phases)

- Windows named-pipe transport (protocol `discovery`/server) — Phase 3+, before any Windows release gate.
- `expected_revision` optimistic concurrency on commands — Phase 1 command handling.
- Multi-writer sequence allocation (`next_sequence` currently assumes the daemon's single-writer discipline) — revisit if a second writer ever appears.

## Exit checklist

Verify each item, in order; every box must be true before opening Phase 1:

- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --workspace` passes with the four named tests green.
- [ ] `codypendent daemon start` starts a detached daemon (log at `<data>/logs/daemon.log`).
- [ ] `codypendent daemon status --json` emits machine-readable status; exit code 0.
- [ ] `codypendent daemon stop` stops it; status then exits 1 with `{"running":false}`.
- [ ] Restart preserves `instance_id` and increments `boot_count` (exit criterion: "daemon restart preserves its instance database").
- [ ] Fixture event log replays into the ledger and back out with an identical projection (exit criterion: "can replay a fixture event log").
- [ ] CI workflow file exists and runs the three gates.
- [ ] All Phase 0 commits are made; working tree clean.
