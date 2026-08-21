//! Workspaces and Teams within an organization.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{OrganizationId, TeamId, UserId, WorkspaceId};
use crate::organization::SlugValidationError;

/// A validated URL-safe slug for teams and workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct TeamSlug(pub String);

impl TeamSlug {
    pub fn new(slug: impl Into<String>) -> Result<Self, SlugValidationError> {
        let s = slug.into();
        if s.len() < 2 || s.len() > 64 {
            return Err(SlugValidationError::InvalidLength(s.len()));
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(SlugValidationError::HyphenBoundary);
        }
        if let Some(pos) = s.as_bytes().iter().position(|&b| !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'-') {
            return Err(SlugValidationError::InvalidChar(s[pos..].chars().next().unwrap()));
        }
        Ok(Self(s))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TeamSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for TeamSlug {
    type Err = SlugValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Team or workspace entity within an organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct Team {
    pub id: TeamId,
    pub organization_id: OrganizationId,
    pub slug: TeamSlug,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
}

/// Request to create a new team or workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct CreateTeamRequest {
    pub slug: TeamSlug,
    pub display_name: String,
}

/// Request to update an existing team or workspace.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct UpdateTeamRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// State of an organization membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum MembershipState {
    #[default]
    Invited,
    Active,
    Suspended,
    /// Unrecognized or newer state. Never treated as active membership.
    #[serde(other)]
    Unknown,
}

impl MembershipState {
    /// Whether the membership confers access. Only the explicit `Active` state qualifies.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// Organization membership binding a user to an organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct OrganizationMembership {
    pub organization_id: OrganizationId,
    pub user_id: UserId,
    pub state: MembershipState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Team member association.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct TeamMember {
    pub team_id: TeamId,
    pub user_id: UserId,
    pub joined_at: DateTime<Utc>,
}

/// Add member to team request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct AddTeamMemberRequest {
    pub user_id: UserId,
}

/// Workspace projection (representing team / workspace environment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct Workspace {
    pub id: WorkspaceId,
    pub organization_id: OrganizationId,
    pub slug: TeamSlug,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
}
