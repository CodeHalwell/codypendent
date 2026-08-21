//! Opaque, sortable identifiers and cryptographic digests for the Control Plane.
//!
//! Entity IDs use UUIDv7 (time-ordered, unique, sortable).
//! Federated repository IDs and digests use 64-character lowercase hexadecimal SHA-256 strings.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum IdValidationError {
    #[error("invalid UUID format: {0}")]
    InvalidUuid(#[from] uuid::Error),
    #[error("invalid hex digest: expected 64 lowercase hex characters, got length {0}")]
    InvalidHexLength(usize),
    #[error("invalid hex character in digest: {0}")]
    InvalidHexChar(char),
}

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Create a new time-ordered (UUIDv7) identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wrap an existing UUID.
            #[must_use]
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// Return the underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdValidationError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }
    };
}

uuid_id!(
    /// Stable identity of a human user in the control plane.
    UserId
);
uuid_id!(
    /// Stable identity of an organization.
    OrganizationId
);
uuid_id!(
    /// Stable identity of a team (or workspace) within an organization.
    TeamId
);
uuid_id!(
    /// Stable identity of a workspace within an organization (alias/equivalent to TeamId).
    WorkspaceId
);
uuid_id!(
    /// Stable identity of a registered repository in the control plane.
    RepositoryId
);
uuid_id!(
    /// Stable identity of a paired daemon instance.
    DaemonId
);
uuid_id!(
    /// Stable identity of an RBAC role grant.
    GrantId
);
uuid_id!(
    /// Stable identity of a synchronization delta receipt.
    SyncReceiptId
);
uuid_id!(
    /// Stable identity of a tombstone record.
    TombstoneId
);
uuid_id!(
    /// Stable identity of an immutable audit record.
    AuditRecordId
);
uuid_id!(
    /// Stable identity of an object in published object storage.
    PublishedObjectId
);
uuid_id!(
    /// Stable identity of a linked external identity (e.g. GitHub, OIDC).
    IdentityId
);
uuid_id!(
    /// Stable identity of a user refresh token credential.
    RefreshTokenId
);
uuid_id!(
    /// Stable identity of a workload credential.
    WorkloadCredentialId
);
uuid_id!(
    /// Stable identity of a pairing challenge.
    ChallengeId
);
uuid_id!(
    /// Stable identity of a registered runner.
    RunnerId
);
uuid_id!(
    /// Stable identity of a remote runner job.
    RunnerJobId
);
uuid_id!(
    /// Stable identity of a runner job execution attempt.
    RunnerAttemptId
);
uuid_id!(
    /// Stable identity of a runner execution lease.
    RunnerLeaseId
);
uuid_id!(
    /// Stable identity of a runner output record.
    RunnerOutputId
);
uuid_id!(
    /// Stable identity of a runner attestation record.
    RunnerAttestationId
);
uuid_id!(
    /// Stable identity of a runner quarantine record.
    RunnerQuarantineId
);
uuid_id!(
    /// Stable identity of a projected shared session.
    SharedSessionId
);
uuid_id!(
    /// Distributed correlation identity for tracing actions across daemons and control plane.
    CorrelationId
);

/// A 64-character lowercase hexadecimal SHA-256 digest representing a repository's
/// cross-machine identity. The control plane never sees local paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct FederatedRepositoryId(pub String);

impl FederatedRepositoryId {
    /// Create a new federated repository ID after validating hex format (64 lowercase hex chars).
    pub fn new(hex_str: impl Into<String>) -> Result<Self, IdValidationError> {
        let s = hex_str.into();
        validate_hex_64(&s)?;
        Ok(Self(s))
    }

    /// Hash arbitrary repository seed bytes to create a valid federated ID.
    #[must_use]
    pub fn from_seed_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let result = hasher.finalize();
        Self(hex::encode(result))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FederatedRepositoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for FederatedRepositoryId {
    type Err = IdValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// A 64-character lowercase hexadecimal SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct Sha256Digest(pub String);

impl Sha256Digest {
    /// Create a new Sha256Digest after validating 64 lowercase hex chars.
    pub fn new(hex_str: impl Into<String>) -> Result<Self, IdValidationError> {
        let s = hex_str.into();
        validate_hex_64(&s)?;
        Ok(Self(s))
    }

    /// Compute the SHA-256 digest over arbitrary input bytes.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let result = hasher.finalize();
        Self(hex::encode(result))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Sha256Digest {
    type Err = IdValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

fn validate_hex_64(s: &str) -> Result<(), IdValidationError> {
    if s.len() != 64 {
        return Err(IdValidationError::InvalidHexLength(s.len()));
    }
    if let Some(pos) = s.as_bytes().iter().position(|&b| !b.is_ascii_hexdigit() || b.is_ascii_uppercase()) {
        return Err(IdValidationError::InvalidHexChar(s[pos..].chars().next().unwrap()));
    }
    Ok(())
}
