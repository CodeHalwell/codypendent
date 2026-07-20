//! Agent profiles (STEP 5.1): the parsed shape of `docs/specs/agent.toml`.
//!
//! A workflow step names an agent *role*; that role resolves to an
//! [`AgentProfile`] — its mode, autonomy, model policy, declared skills and
//! tools, its capability permissions, its budget slice, and the completion
//! conditions it must satisfy. This module is the parser; resolving a role to a
//! profile against the registry and enforcing the permissions at run time are the
//! runtime's job.

use serde::{Deserialize, Serialize};

/// The agent-profile schema version this build understands.
pub const SUPPORTED_AGENT_SCHEMA_VERSION: u32 = 1;

/// A parse/validation failure for an agent profile.
#[derive(Debug, thiserror::Error)]
pub enum AgentProfileError {
    #[error("invalid agent profile: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("unsupported agent schema_version {found} (this build supports {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("agent profile id must not be empty")]
    EmptyId,
}

/// Parse an agent profile from TOML and validate its schema version + id.
pub fn parse_agent_profile(toml_str: &str) -> Result<AgentProfile, AgentProfileError> {
    let profile: AgentProfile = toml::from_str(toml_str)?;
    if profile.schema_version != SUPPORTED_AGENT_SCHEMA_VERSION {
        return Err(AgentProfileError::UnsupportedSchemaVersion {
            found: profile.schema_version,
            supported: SUPPORTED_AGENT_SCHEMA_VERSION,
        });
    }
    if profile.id.trim().is_empty() {
        return Err(AgentProfileError::EmptyId);
    }
    Ok(profile)
}

/// A declarative agent profile (the shape of `docs/specs/agent.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub schema_version: u32,
    /// The profile's stable id (e.g. `code.implementer`).
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The workflow *role* this profile fulfils (e.g. `implementer`). A workflow
    /// step names a short role; this is how a profile declares which role it
    /// serves. When absent, the role defaults to the last dotted segment of
    /// `id` — so `code.implementer` fulfils `implementer` — see
    /// [`AgentProfile::fulfilled_role`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The scope the profile applies at (e.g. `repository`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The agent mode (e.g. `build`, `explore`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The autonomy level (e.g. `supervised`, `autonomous`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy: Option<String>,
    /// The model-selection policy (e.g. `coding-balanced`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_policy: Option<String>,
    /// The skills the agent is granted.
    #[serde(default)]
    pub skills: Vec<String>,
    /// The tools the agent may use.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The agent's capability permissions.
    #[serde(default)]
    pub permissions: AgentPermissions,
    /// The agent's budget slice.
    #[serde(default)]
    pub budget: AgentBudget,
    /// The conditions the agent must satisfy to complete.
    #[serde(default)]
    pub completion: AgentCompletion,
}

impl AgentProfile {
    /// The workflow role this profile fulfils: its explicit [`role`](Self::role)
    /// when set, otherwise the last dotted segment of its [`id`](Self::id) (the
    /// whole id when it has no dot). This is the key a workflow step's short
    /// `role` resolves against — a manifest's `role: implementer` binds to the
    /// canonical `id = "code.implementer"` profile through the suffix fallback,
    /// while an explicit `role` lets a profile whose id does not end in the role
    /// name still serve it.
    #[must_use]
    pub fn fulfilled_role(&self) -> &str {
        match self.role.as_deref() {
            Some(role) => role,
            // `rsplit` on a non-empty string always yields at least one segment;
            // the id is validated non-empty, so this never falls through.
            None => self.id.rsplit('.').next().unwrap_or(self.id.as_str()),
        }
    }
}

/// The capability permissions an agent declares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPermissions {
    #[serde(default)]
    pub filesystem_read: Vec<String>,
    #[serde(default)]
    pub filesystem_write: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    /// Whether the agent may spawn sub-agents.
    #[serde(default)]
    pub delegate_agents: bool,
}

/// The budget slice allotted to an agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_duration_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_tool_calls: Option<u64>,
}

/// The completion conditions an agent must satisfy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCompletion {
    #[serde(default)]
    pub requires: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_canonical_agent_profile_parses() {
        let profile = parse_agent_profile(include_str!("../../../docs/specs/agent.toml")).unwrap();
        assert_eq!(profile.id, "code.implementer");
        assert_eq!(profile.mode.as_deref(), Some("build"));
        assert_eq!(profile.autonomy.as_deref(), Some("supervised"));
        assert!(profile.skills.contains(&"code.implement".to_owned()));
        assert!(profile.tools.contains(&"workspace.apply_patch".to_owned()));
        // Permissions: writes are scoped to the worktree; no delegation.
        assert_eq!(profile.permissions.filesystem_write, vec!["$WORKTREE"]);
        assert!(!profile.permissions.delegate_agents);
        assert!(profile.permissions.commands.contains(&"cargo".to_owned()));
        // Budget + completion carried through.
        assert_eq!(profile.budget.maximum_tool_calls, Some(80));
        assert!(profile
            .completion
            .requires
            .contains(&"targeted-tests-pass".to_owned()));
    }

    #[test]
    fn fulfilled_role_defaults_to_the_last_dotted_id_segment() {
        // The canonical profile has no explicit role, so it fulfils the short
        // role `implementer` (the last segment of `code.implementer`) — the exact
        // role the `repair-github-check` manifest's `patch` step names.
        let profile = parse_agent_profile(include_str!("../../../docs/specs/agent.toml")).unwrap();
        assert_eq!(profile.role, None);
        assert_eq!(profile.fulfilled_role(), "implementer");
    }

    #[test]
    fn an_explicit_role_overrides_the_id_suffix() {
        let toml = "schema_version = 1\nid = \"code.impl\"\nname = \"A\"\nrole = \"implementer\"\n";
        let profile = parse_agent_profile(toml).unwrap();
        assert_eq!(profile.fulfilled_role(), "implementer");

        // A dotless id fulfils itself.
        let toml = "schema_version = 1\nid = \"reviewer\"\nname = \"R\"\n";
        assert_eq!(
            parse_agent_profile(toml).unwrap().fulfilled_role(),
            "reviewer"
        );
    }

    #[test]
    fn rejects_an_unsupported_schema_version() {
        let toml = "schema_version = 99\nid = \"a\"\nname = \"A\"\n";
        assert!(matches!(
            parse_agent_profile(toml),
            Err(AgentProfileError::UnsupportedSchemaVersion { found: 99, .. })
        ));
    }

    #[test]
    fn rejects_an_empty_id() {
        let toml = "schema_version = 1\nid = \"\"\nname = \"A\"\n";
        assert!(matches!(
            parse_agent_profile(toml),
            Err(AgentProfileError::EmptyId)
        ));
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        // deny_unknown_fields guards against typo'd keys.
        let toml = "schema_version = 1\nid = \"a\"\nname = \"A\"\nmoed = \"build\"\n";
        assert!(parse_agent_profile(toml).is_err());
    }
}
