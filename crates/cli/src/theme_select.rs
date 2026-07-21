//! Live-TUI theme selection wiring (STEP 6.6).
//!
//! `codypendent-tui` ships the pure decision logic — six accessibility
//! variants, [`ColorDepth::detect`] (`NO_COLOR`/`COLORTERM`/`TERM`),
//! [`Theme::select`] (manual override always wins), and the data-only
//! theme-pack loader ([`load_theme_pack`], which structurally rejects any
//! pack declaring capabilities/permissions) — but nothing in the live TUI
//! ever called it: `tui::run` hardcoded `Theme::dark()`. This module is the
//! seam that calls the real API with real inputs (an optional `--theme`
//! name, resolved against the terminal's detected color depth or an on-disk
//! theme pack).
//!
//! There is no general user-facing config file yet (the only precedent is
//! `SessionStore` in `tui.rs`, which is an internal resume-token cache, not
//! user preferences), so the override surface is `--theme <NAME>` /
//! `CODYPENDENT_THEME`, following the existing `CODYPENDENT_DATA_DIR` /
//! `CODYPENDENT_SOCKET` env-var and `--repo`/`--mode`-style flag
//! conventions (see `main.rs`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_tui::{load_theme_pack, ColorDepth, Theme, ThemePreferences, ThemeVariant};

/// Data-only theme packs (STEP 6.6) load from `<data-dir>/themes/<id>.toml` —
/// the existing data-dir convention (see the module docs on
/// `RuntimePaths`), alongside the CLI's other ad hoc data-dir paths (e.g.
/// `SessionStore::file` in `tui.rs`) rather than a new `RuntimePaths` field,
/// since this is the only caller of an optional, read-only lookup.
fn theme_pack_path(paths: &RuntimePaths, id: &str) -> PathBuf {
    paths.data_dir.join("themes").join(format!("{id}.toml"))
}

/// Match a `--theme`/`CODYPENDENT_THEME` value against a built-in variant
/// name, case- and separator-insensitively (`High-Contrast`, `high_contrast`,
/// and `HIGHCONTRAST` all resolve to the same variant).
fn parse_builtin_variant(name: &str) -> Option<ThemeVariant> {
    let normalized = name.to_ascii_lowercase().replace(['-', '_'], "");
    match normalized.as_str() {
        "dark" => Some(ThemeVariant::Dark),
        "light" => Some(ThemeVariant::Light),
        "highcontrast" => Some(ThemeVariant::HighContrast),
        "colorblindsafe" => Some(ThemeVariant::ColorBlindSafe),
        "ansi256" => Some(ThemeVariant::Ansi256),
        "ansi16" => Some(ThemeVariant::Ansi16),
        "monochrome" | "mono" => Some(ThemeVariant::Monochrome),
        _ => None,
    }
}

/// Load a named theme pack from `<data-dir>/themes/<id>.toml`.
fn load_pack(paths: &RuntimePaths, id: &str) -> Result<Theme> {
    let path = theme_pack_path(paths, id);
    let toml_str = std::fs::read_to_string(&path).with_context(|| {
        let path_display = path.display();
        format!(
            "theme `{id}` is not a built-in variant (dark, light, high-contrast, \
             color-blind-safe, ansi256, ansi16, monochrome) and no theme pack was \
             found at {path_display}"
        )
    })?;
    let path_display = path.display();
    load_theme_pack(&toml_str)
        .map_err(|e| anyhow::anyhow!("theme pack `{id}` at {path_display}: {e}"))
}

/// Resolve the live TUI's theme for an explicit terminal color `depth` and an
/// optional override name. Split from [`resolve_theme`] so tests can supply
/// `depth` directly instead of depending on the process environment —
/// `ColorDepth::detect`'s own env-parsing rules are already covered by
/// `codypendent-tui`'s own tests; what is under test *here* is the wiring:
/// given a depth and an override, does the live TUI construction path pick
/// the theme it should.
///
/// Precedence (matches `Theme::select`'s "manual override always wins"
/// contract): an override name, when given, always wins over `depth`. A name
/// matching a built-in variant selects that variant outright; any other name
/// is looked up as a theme-pack id under `<data-dir>/themes/<name>.toml` and
/// loaded via `codypendent_tui::load_theme_pack`. With no override, `depth`
/// alone picks the built-in variant.
pub fn resolve_theme_for_depth(
    paths: &RuntimePaths,
    depth: ColorDepth,
    override_name: Option<&str>,
) -> Result<Theme> {
    let Some(name) = override_name else {
        return Ok(Theme::select(depth, ThemePreferences::default()));
    };
    if let Some(variant) = parse_builtin_variant(name) {
        return Ok(Theme::select(
            depth,
            ThemePreferences {
                override_variant: Some(variant),
                ..ThemePreferences::default()
            },
        ));
    }
    load_pack(paths, name)
}

/// The live entry point: detect the terminal's real color depth
/// ([`ColorDepth::detect`], honoring `NO_COLOR`/`COLORTERM`/`TERM`) and
/// resolve the theme for it (see [`resolve_theme_for_depth`]).
pub fn resolve_theme(paths: &RuntimePaths, override_name: Option<&str>) -> Result<Theme> {
    resolve_theme_for_depth(paths, ColorDepth::detect(), override_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn paths_in(dir: &Path) -> RuntimePaths {
        RuntimePaths::from_data_dir(dir.to_path_buf())
    }

    #[test]
    fn no_override_selects_by_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(
            resolve_theme_for_depth(&paths, ColorDepth::Monochrome, None).unwrap(),
            Theme::monochrome()
        );
        assert_eq!(
            resolve_theme_for_depth(&paths, ColorDepth::TrueColor, None).unwrap(),
            Theme::dark()
        );
        assert_eq!(
            resolve_theme_for_depth(&paths, ColorDepth::Ansi256, None).unwrap(),
            Theme::ansi256()
        );
    }

    #[test]
    fn builtin_override_wins_over_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        // A monochrome depth (as NO_COLOR would force) must still lose to an
        // explicit --theme, matching `Theme::select`'s own override contract.
        let theme = resolve_theme_for_depth(&paths, ColorDepth::Monochrome, Some("light")).unwrap();
        assert_eq!(theme, Theme::light());
    }

    #[test]
    fn builtin_override_name_is_case_and_separator_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        for name in [
            "High-Contrast",
            "high_contrast",
            "HIGHCONTRAST",
            "highcontrast",
        ] {
            let theme = resolve_theme_for_depth(&paths, ColorDepth::TrueColor, Some(name)).unwrap();
            assert_eq!(theme, Theme::high_contrast(), "failed for {name}");
        }
    }

    #[test]
    fn unknown_name_falls_back_to_a_theme_pack_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let themes_dir = tmp.path().join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        std::fs::write(
            themes_dir.join("solarish.toml"),
            r##"
schema_version = 1
id = "solarish"
base = "dark"
[tokens]
"status.error" = "#ff0000"
"##,
        )
        .unwrap();

        let theme =
            resolve_theme_for_depth(&paths, ColorDepth::TrueColor, Some("solarish")).unwrap();
        assert_eq!(theme.status.error, ratatui::style::Color::Rgb(0xff, 0, 0));
        // Untouched tokens still fall back to the pack's declared base.
        assert_eq!(theme.text.primary, Theme::dark().text.primary);
    }

    #[test]
    fn a_pack_declaring_capabilities_is_rejected_not_silently_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let themes_dir = tmp.path().join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        std::fs::write(
            themes_dir.join("malicious.toml"),
            r#"
schema_version = 1
id = "malicious"
[capabilities]
network = ["evil.example.com:443"]
"#,
        )
        .unwrap();

        let err =
            resolve_theme_for_depth(&paths, ColorDepth::TrueColor, Some("malicious")).unwrap_err();
        assert!(
            err.to_string().contains("malicious"),
            "expected the pack id in the error, got: {err}"
        );
    }

    #[test]
    fn an_unresolvable_name_is_a_clear_error_not_a_silent_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let err = resolve_theme_for_depth(&paths, ColorDepth::TrueColor, Some("nonexistent"))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nonexistent"), "{msg}");
    }
}
