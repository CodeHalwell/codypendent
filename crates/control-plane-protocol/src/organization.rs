//! Organization models, requests, and validation.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ids::OrganizationId;
use crate::publication::{DataClassification, PublicationClass};

#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum SlugValidationError {
    #[error("slug length must be between 2 and 64 characters, got {0}")]
    InvalidLength(usize),
    #[error(
        "slug contains invalid character '{0}': only lowercase alphanumeric and '-' are allowed"
    )]
    InvalidChar(char),
    #[error("slug cannot start or end with a hyphen")]
    HyphenBoundary,
}

/// A validated URL-safe slug for organizations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct OrganizationSlug(pub String);

impl OrganizationSlug {
    pub fn new(slug: impl Into<String>) -> Result<Self, SlugValidationError> {
        let s = slug.into();
        validate_slug(&s)?;
        Ok(Self(s))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrganizationSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for OrganizationSlug {
    type Err = SlugValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

fn validate_slug(s: &str) -> Result<(), SlugValidationError> {
    if s.len() < 2 || s.len() > 64 {
        return Err(SlugValidationError::InvalidLength(s.len()));
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err(SlugValidationError::HyphenBoundary);
    }
    if let Some(pos) = s.as_bytes().iter().position(|&b| !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'-') {
        return Err(SlugValidationError::InvalidChar(s[pos..].chars().next().unwrap()));
    }
    Ok(())
}

/// Core Organization entity in the control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct Organization {
    pub id: OrganizationId,
    pub slug: OrganizationSlug,
    pub display_name: String,
    pub max_publication_class: PublicationClass,
    pub max_classification: DataClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_residency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
    pub policy_version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Request to create a new organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct CreateOrganizationRequest {
    pub slug: OrganizationSlug,
    pub display_name: String,
    #[serde(default)]
    pub max_publication_class: Option<PublicationClass>,
    #[serde(default)]
    pub max_classification: Option<DataClassification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_residency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

/// Request to update an existing organization.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct UpdateOrganizationRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_publication_class: Option<PublicationClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_classification: Option<DataClassification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_residency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

/// Compact summary of an organization for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct OrganizationSummary {
    pub id: OrganizationId,
    pub slug: OrganizationSlug,
    pub display_name: String,
    pub max_publication_class: PublicationClass,
    pub member_count: u32,
    pub repository_count: u32,
    pub created_at: DateTime<Utc>,
}
