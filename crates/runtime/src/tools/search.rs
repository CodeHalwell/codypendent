//! `workspace.search` — ripgrep over the granted read scope, parsed into typed
//! matches.

use std::path::PathBuf;
use std::process::Stdio;

use codypendent_daemon::policy::{PathScope, ScopeVerdict};
use codypendent_protocol::ProposedAction;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::{CapabilityKind, ToolError};

/// Maximum matches returned; beyond this the search stops and flags truncation.
const MATCH_CAP: usize = 200;

/// Typed input for [`Search::execute`].
#[derive(Debug, Clone)]
pub struct SearchInput {
    /// The ripgrep pattern (regex).
    pub pattern: String,
    /// An optional ripgrep glob filter (e.g. `*.rs`).
    pub glob: Option<String>,
}

/// One ripgrep match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    /// The file the match is in.
    pub path: PathBuf,
    /// 1-based line number.
    pub line_number: u64,
    /// The matched line, trailing newline stripped.
    pub line: String,
}

/// The result of a [`Search::execute`] call.
#[derive(Debug, Clone)]
pub struct SearchResults {
    /// Matches, capped at [`MATCH_CAP`].
    pub matches: Vec<SearchMatch>,
    /// Whether the cap was hit (more matches exist).
    pub truncated: bool,
}

/// The `workspace.search` tool.
pub struct Search;

impl Search {
    /// The stable tool name.
    pub const NAME: &'static str = "workspace.search";

    /// Capability classes this tool draws on.
    pub fn required_capabilities() -> &'static [CapabilityKind] {
        &[CapabilityKind::FileRead]
    }

    /// The [`ProposedAction`] the middleware evaluates before granting: reading
    /// the scope's roots.
    pub fn proposed_action(scope: &PathScope) -> ProposedAction {
        ProposedAction::ReadFiles {
            paths: scope
                .roots
                .iter()
                .map(|r| r.to_string_lossy().into_owned())
                .collect(),
        }
    }

    /// Search the granted scope's roots for `input.pattern`, returning at most
    /// [`MATCH_CAP`] typed matches. The search is confined to the scope: only the
    /// scope roots are handed to ripgrep, and any match whose path resolves
    /// outside the scope (or into the deny list) is dropped defensively.
    pub async fn execute(
        input: &SearchInput,
        scope: &PathScope,
    ) -> Result<SearchResults, ToolError> {
        if scope.roots.is_empty() {
            return Ok(SearchResults {
                matches: Vec::new(),
                truncated: false,
            });
        }

        let mut command = tokio::process::Command::new("rg");
        command
            .arg("--json")
            .arg("-n")
            .arg("--no-config")
            .arg("--no-messages");
        if let Some(glob) = &input.glob {
            command.arg("--glob").arg(glob);
        }
        command.arg("--regexp").arg(&input.pattern);
        for root in &scope.roots {
            command.arg(root);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::ProgramNotFound("rg".to_string())
            } else {
                ToolError::Io(e)
            }
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Other(anyhow::anyhow!("rg stdout unavailable")))?;
        let mut reader = BufReader::new(stdout).lines();

        let mut matches = Vec::new();
        let mut truncated = false;
        while let Some(line) = reader.next_line().await? {
            let Ok(event) = serde_json::from_str::<RgEvent>(&line) else {
                continue;
            };
            if event.kind != "match" {
                continue;
            }
            let Some(data) = event.data else { continue };
            let (Some(path), Some(line_number), Some(text)) =
                (data.path, data.line_number, data.lines)
            else {
                continue;
            };
            let path = PathBuf::from(path.text);
            // Defensive scope confinement even though rg was pointed at roots.
            if !matches!(scope.classify(&path), ScopeVerdict::Allowed) {
                continue;
            }
            matches.push(SearchMatch {
                path,
                line_number,
                line: text.text.trim_end_matches(['\n', '\r']).to_string(),
            });
            if matches.len() >= MATCH_CAP {
                truncated = true;
                break;
            }
        }

        // Stop ripgrep early if we hit the cap, then reap.
        let _ = child.start_kill();
        let _ = child.wait().await;

        Ok(SearchResults { matches, truncated })
    }
}

/// A single `rg --json` event line. Only the `match` shape is consumed.
#[derive(Debug, Deserialize)]
struct RgEvent {
    #[serde(rename = "type")]
    kind: String,
    data: Option<RgData>,
}

#[derive(Debug, Deserialize)]
struct RgData {
    path: Option<RgText>,
    lines: Option<RgText>,
    line_number: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RgText {
    #[serde(default)]
    text: String,
}
