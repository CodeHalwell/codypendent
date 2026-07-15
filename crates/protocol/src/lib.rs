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
