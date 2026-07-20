//! Theme packs (STEP 6.6): data-only theme plugins.
//!
//! A theme pack is a **data-only** plugin — it ships semantic-token colors and
//! nothing executable. The README rule is absolute: *"Theme plugins must not
//! receive execution permissions."* This module enforces it structurally: a
//! `theme.toml` that declares **any** capability or permission (filesystem,
//! network, secrets, subprocess, or a free-form `[permissions]` table) is
//! rejected at load, before it can register a single color. There is no code
//! path by which a theme pack acquires an execution capability.
//!
//! A valid pack overrides only the semantic tokens it names; unspecified tokens
//! fall back to a chosen base variant, so a pack is a small, safe delta over a
//! built-in theme rather than a full palette every author must restate.

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use crate::theme::{Theme, ThemeVariant};

/// The theme-pack schema version this build understands.
pub const SUPPORTED_THEME_PACK_SCHEMA_VERSION: u32 = 1;

/// Why a theme pack was rejected.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ThemePackError {
    #[error("invalid theme pack: {0}")]
    Parse(String),
    #[error("unsupported theme-pack schema_version {found} (this build supports {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("theme pack id must not be empty")]
    EmptyId,
    /// The security invariant: a theme pack declared an execution capability.
    #[error("theme packs must not request execution permissions, but this pack declares: {0}")]
    ExecutionPermissionsForbidden(String),
    #[error("invalid color `{value}` for token `{token}` (expected `#rrggbb` or an index 0-255)")]
    InvalidColor { token: String, value: String },
}

/// A parsed theme-pack manifest. The `capabilities`/`permissions` fields exist
/// **only** so a pack that illegitimately declares them parses and is then
/// explicitly rejected — a clearer failure than a bare unknown-field error, and
/// proof the invariant is checked rather than merely assumed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemePackManifest {
    pub schema_version: u32,
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// The built-in variant the pack layers its overrides on top of.
    #[serde(default)]
    pub base: Option<ThemeVariant>,
    /// Token overrides: `"status.error" = "#ff0000"`, `"surface.panel" = "236"`.
    #[serde(default)]
    pub tokens: std::collections::BTreeMap<String, String>,
    /// Present only to be rejected — a theme pack gets no filesystem/network/etc.
    #[serde(default)]
    pub capabilities: Option<toml::Value>,
    /// Present only to be rejected — a theme pack gets no permissions table.
    #[serde(default)]
    pub permissions: Option<toml::Value>,
}

/// Parse and validate a theme pack, returning the resolved [`Theme`].
///
/// Rejects (in order): a bad schema version, an empty id, **any** declared
/// capability/permission (the execution-permission ban), and any malformed color
/// override. On success, the pack's token overrides are applied over its declared
/// base variant (default [`ThemeVariant::Dark`]).
pub fn load_theme_pack(toml_str: &str) -> Result<Theme, ThemePackError> {
    let manifest: ThemePackManifest =
        toml::from_str(toml_str).map_err(|e| ThemePackError::Parse(e.to_string()))?;
    validate_manifest(&manifest)?;
    apply_tokens(&manifest)
}

/// Validate a parsed manifest without resolving colors — the security gate on its
/// own, so a caller can check a pack is permission-free independently.
pub fn validate_manifest(manifest: &ThemePackManifest) -> Result<(), ThemePackError> {
    if manifest.schema_version != SUPPORTED_THEME_PACK_SCHEMA_VERSION {
        return Err(ThemePackError::UnsupportedSchemaVersion {
            found: manifest.schema_version,
            supported: SUPPORTED_THEME_PACK_SCHEMA_VERSION,
        });
    }
    if manifest.id.trim().is_empty() {
        return Err(ThemePackError::EmptyId);
    }
    // The invariant. A theme pack that names *any* capability or permission is
    // refused outright — themes are data, never executable.
    let mut declared = Vec::new();
    if manifest.capabilities.is_some() {
        declared.push("capabilities");
    }
    if manifest.permissions.is_some() {
        declared.push("permissions");
    }
    if !declared.is_empty() {
        return Err(ThemePackError::ExecutionPermissionsForbidden(
            declared.join(", "),
        ));
    }
    Ok(())
}

fn apply_tokens(manifest: &ThemePackManifest) -> Result<Theme, ThemePackError> {
    let mut theme = Theme::variant(manifest.base.unwrap_or(ThemeVariant::Dark));
    for (token, value) in &manifest.tokens {
        let color = parse_color(token, value)?;
        set_token(&mut theme, token, color).ok_or_else(|| ThemePackError::InvalidColor {
            token: token.clone(),
            value: value.clone(),
        })?;
    }
    Ok(theme)
}

/// Parse a color override: `#rrggbb` hex, or a bare `0-255` palette index.
fn parse_color(token: &str, value: &str) -> Result<Color, ThemePackError> {
    let v = value.trim();
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() == 6 {
            if let Ok(rgb) = u32::from_str_radix(hex, 16) {
                let [_, r, g, b] = rgb.to_be_bytes();
                return Ok(Color::Rgb(r, g, b));
            }
        }
        return Err(ThemePackError::InvalidColor {
            token: token.to_string(),
            value: value.to_string(),
        });
    }
    if let Ok(idx) = v.parse::<u8>() {
        return Ok(Color::Indexed(idx));
    }
    Err(ThemePackError::InvalidColor {
        token: token.to_string(),
        value: value.to_string(),
    })
}

/// Set a semantic token by its dotted name (`group.field`). Returns `None` for an
/// unknown token name.
fn set_token(theme: &mut Theme, token: &str, color: Color) -> Option<()> {
    match token {
        "surface.background" => theme.surface.background = color,
        "surface.panel" => theme.surface.panel = color,
        "surface.border" => theme.surface.border = color,
        "surface.overlay" => theme.surface.overlay = color,
        "text.primary" => theme.text.primary = color,
        "text.secondary" => theme.text.secondary = color,
        "text.muted" => theme.text.muted = color,
        "text.heading" => theme.text.heading = color,
        "status.info" => theme.status.info = color,
        "status.success" => theme.status.success = color,
        "status.warning" => theme.status.warning = color,
        "status.error" => theme.status.error = color,
        "status.running" => theme.status.running = color,
        "status.idle" => theme.status.idle = color,
        "syntax.keyword" => theme.syntax.keyword = color,
        "syntax.literal" => theme.syntax.literal = color,
        "syntax.string" => theme.syntax.string = color,
        "syntax.comment" => theme.syntax.comment = color,
        "diff.added" => theme.diff.added = color,
        "diff.removed" => theme.diff.removed = color,
        "diff.context" => theme.diff.context = color,
        "diff.header" => theme.diff.header = color,
        "agent.model_text" => theme.agent.model_text = color,
        "agent.tool" => theme.agent.tool = color,
        "agent.thinking" => theme.agent.thinking = color,
        "focus.active" => theme.focus.active = color,
        "focus.inactive" => theme.focus.inactive = color,
        "selection.foreground" => theme.selection.foreground = color,
        "selection.background" => theme.selection.background = color,
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_data_only_pack_loads() {
        let toml = r##"
schema_version = 1
id = "solarish"
name = "Solarish"
base = "dark"
[tokens]
"status.error" = "#ff0000"
"surface.panel" = "236"
"##;
        let theme = load_theme_pack(toml).expect("data-only pack loads");
        assert_eq!(theme.status.error, Color::Rgb(0xff, 0x00, 0x00));
        assert_eq!(theme.surface.panel, Color::Indexed(236));
        // Untouched tokens keep the base variant's values.
        assert_eq!(theme.text.primary, Theme::dark().text.primary);
    }

    #[test]
    fn pack_declaring_capabilities_is_rejected() {
        let toml = r#"
schema_version = 1
id = "malicious"
[capabilities]
network = ["evil.example.com:443"]
"#;
        let err = load_theme_pack(toml).unwrap_err();
        assert!(
            matches!(err, ThemePackError::ExecutionPermissionsForbidden(ref s) if s.contains("capabilities")),
            "got {err:?}"
        );
    }

    #[test]
    fn pack_declaring_permissions_is_rejected() {
        let toml = r#"
schema_version = 1
id = "malicious"
[permissions]
filesystem_read = ["/etc/passwd"]
"#;
        let err = load_theme_pack(toml).unwrap_err();
        assert!(matches!(
            err,
            ThemePackError::ExecutionPermissionsForbidden(_)
        ));
    }

    #[test]
    fn base_variant_selects_the_starting_palette() {
        let toml = r#"
schema_version = 1
id = "hc-tweak"
base = "high-contrast"
"#;
        let theme = load_theme_pack(toml).expect("loads");
        assert_eq!(theme, Theme::high_contrast());
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let toml = "schema_version = 2\nid = \"x\"\n";
        assert!(matches!(
            load_theme_pack(toml),
            Err(ThemePackError::UnsupportedSchemaVersion { found: 2, .. })
        ));
    }

    #[test]
    fn empty_id_is_rejected() {
        let toml = "schema_version = 1\nid = \"\"\n";
        assert_eq!(load_theme_pack(toml), Err(ThemePackError::EmptyId));
    }

    #[test]
    fn malformed_color_is_rejected() {
        let toml = r#"
schema_version = 1
id = "bad"
[tokens]
"status.error" = "not-a-color"
"#;
        assert!(matches!(
            load_theme_pack(toml),
            Err(ThemePackError::InvalidColor { .. })
        ));
    }

    #[test]
    fn unknown_token_name_is_rejected() {
        let toml = r##"
schema_version = 1
id = "bad"
[tokens]
"surface.nonexistent" = "#ffffff"
"##;
        assert!(matches!(
            load_theme_pack(toml),
            Err(ThemePackError::InvalidColor { .. })
        ));
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        // deny_unknown_fields stops a pack from smuggling a `[runtime]` table.
        let toml = r#"
schema_version = 1
id = "sneaky"
[runtime]
command = "rm -rf /"
"#;
        assert!(matches!(
            load_theme_pack(toml),
            Err(ThemePackError::Parse(_))
        ));
    }
}
