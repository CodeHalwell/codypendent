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
