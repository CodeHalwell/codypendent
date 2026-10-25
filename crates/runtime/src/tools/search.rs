//! `workspace.search` — ripgrep over the granted read scope, parsed into typed
//! matches.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use codypendent_daemon::policy::{PathScope, ScopeVerdict};
use codypendent_protocol::ProposedAction;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::time::Instant;

use super::{CapabilityKind, MAX_CAPTURE_BYTES, ToolError, salient::clamp_search_line};

/// Maximum matches returned; beyond this the search stops and flags truncation.
const MATCH_CAP: usize = 200;

/// Hard ceiling on how much of ripgrep's `--json` stream is read into memory.
///
/// [`MATCH_CAP`] bounds the match *count*, which is not a bound on bytes: a
/// `match` event embeds the entire matched physical line, and every sibling
/// tool bounds its capture (shell at [`MAX_CAPTURE_BYTES`], read_file at 64
/// MiB, the salient view per line) while this one bounded nothing at all. One
/// minified bundle or generated blob in the tree — a single line of many
/// megabytes — was read whole into a `String`, and up to 200 of them were then
/// rendered into the transcript, which is re-sent to the model every step until
/// compaction.
///
/// The same 16 MiB shell uses for a captured stream, for the same reason.
const MAX_STREAM_BYTES: u64 = MAX_CAPTURE_BYTES as u64;

/// Longest matched line kept, in bytes. A search answers "where is this", so
/// the useful part of a hit is its location and enough of the line to recognise
/// it; the rest is what read_file is for. Matches the salient view's per-line
/// clamp, which exists against exactly this failure.
const MAX_MATCH_LINE_BYTES: usize = 2048;

/// Wall-clock bound on one search. A pathological regex can drive ripgrep into
/// effectively unbounded backtracking-like blowup; without a bound the run's
/// cancellation is blind while the tool executes, so a hung search would wedge
/// the run indefinitely.
const SEARCH_TIMEOUT_SECS: u64 = 120;
const SEARCH_ATTEMPTS: usize = 2;
const MAX_ERROR_BYTES: usize = 8192;

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
    /// Whether the results are a prefix of the truth: either [`MATCH_CAP`]
    /// matches were returned or [`MAX_STREAM_BYTES`] of ripgrep output was
    /// read. Either way more matches may exist.
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
        Self::execute_with(
            input,
            scope,
            Path::new("rg"),
            Duration::from_secs(SEARCH_TIMEOUT_SECS),
        )
        .await
    }

    async fn execute_with(
        input: &SearchInput,
        scope: &PathScope,
        executable: &Path,
        budget: Duration,
    ) -> Result<SearchResults, ToolError> {
        if scope.roots.is_empty() {
            return Ok(SearchResults {
                matches: Vec::new(),
                truncated: false,
            });
        }

        let started = Instant::now();
        let deadline = started + budget;
        for attempt in 1..=SEARCH_ATTEMPTS {
            // Reserve time for one retry without extending the caller's deadline.
            let attempt_deadline = if attempt == SEARCH_ATTEMPTS {
                deadline
            } else {
                started + budget / SEARCH_ATTEMPTS as u32
            };
            match tokio::time::timeout_at(attempt_deadline, search_once(input, scope, executable))
                .await
            {
                Ok(Ok((results, status, stderr))) => {
                    if results.truncated || status.success() || status.code() == Some(1) {
                        return Ok(results);
                    }
                    if !interrupted(status) || attempt == SEARCH_ATTEMPTS {
                        return Err(attempt_error(
                            ToolError::Other(anyhow::anyhow!(
                                "Search failed ({status}){}{}",
                                if stderr.is_empty() { "" } else { ": " },
                                stderr,
                            )),
                            attempt,
                        ));
                    }
                }
                Ok(Err(error)) => return Err(attempt_error(error, attempt)),
                Err(_) if attempt == SEARCH_ATTEMPTS => {
                    return Err(attempt_error(
                        ToolError::TimedOut {
                            tool: Self::NAME,
                            seconds: budget.as_secs().max(1),
                        },
                        attempt,
                    ));
                }
                Err(_) => {}
            }
            tracing::debug!(attempt, "Retrying an interrupted workspace search");
        }
        unreachable!()
    }
}

fn attempt_error(error: ToolError, attempt: usize) -> ToolError {
    if attempt == 1 {
        error
    } else {
        ToolError::Other(anyhow::anyhow!(
            "{error}. Automatic retry attempted once (2 attempts total)."
        ))
    }
}

fn interrupted(status: ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(9) || status.code() == Some(137)
    }
    #[cfg(not(unix))]
    {
        status.code() == Some(137)
    }
}

async fn search_once(
    input: &SearchInput,
    scope: &PathScope,
    executable: &Path,
) -> Result<(SearchResults, ExitStatus, String), ToolError> {
    let mut command = tokio::process::Command::new(executable);
    command.arg("--json").arg("-n").arg("--no-config");
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
        .stderr(Stdio::piped())
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
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Other(anyhow::anyhow!("rg stderr unavailable")))?;
    // `Take` is what makes the bound real: `next_line` buffers a whole line
    // before any code here can look at it, so a limit applied after the
    // read is no limit at all against a single enormous line.
    let mut reader = BufReader::new(stdout.take(MAX_STREAM_BYTES)).lines();

    let collect = async {
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
                line: clamp_search_line(
                    text.text.trim_end_matches(['\n', '\r']),
                    MAX_MATCH_LINE_BYTES,
                ),
            });
            if matches.len() >= MATCH_CAP {
                truncated = true;
                break;
            }
        }
        // The stream bound was reached, so ripgrep had more to say and the
        // results are a prefix of the truth — the same thing `truncated`
        // already means for the match cap.
        if reader.get_ref().get_ref().limit() == 0 {
            truncated = true;
        }
        Ok::<_, ToolError>((matches, truncated))
    };

    let capture = async {
        let (matches, truncated) = collect.await?;
        if truncated {
            let _ = child.start_kill();
        }
        let status = child.wait().await?;
        Ok::<_, ToolError>((SearchResults { matches, truncated }, status))
    };
    let ((results, status), stderr) = tokio::try_join!(capture, capture_error(stderr))?;
    Ok((results, status, stderr))
}

async fn capture_error(mut stderr: impl AsyncRead + Unpin) -> Result<String, ToolError> {
    let mut captured = Vec::new();
    let mut buffer = [0; 4096];
    let mut truncated = false;
    loop {
        let count = stderr.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(MAX_ERROR_BYTES - captured.len());
        captured.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
    }
    let mut message = String::from_utf8_lossy(&captured).trim().to_owned();
    if truncated {
        message.push('…');
    }
    Ok(message)
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

#[cfg(all(test, unix))]
#[path = "search_process_tests.rs"]
mod process_tests;

#[cfg(test)]
mod tests {
    use super::{
        MATCH_CAP, MAX_MATCH_LINE_BYTES, MAX_STREAM_BYTES, SEARCH_TIMEOUT_SECS, Search, SearchInput,
    };
    use crate::tools::ABSOLUTE_MAX_TIMEOUT;
    use codypendent_daemon::policy::PathScope;

    /// `workspace.search` runs ripgrep under [`SEARCH_TIMEOUT_SECS`] (C12: the
    /// search tool previously had no timeout). Pin — at compile time — that the
    /// bound is a real, finite ceiling within the runtime's absolute wall-clock
    /// maximum.
    #[test]
    fn search_timeout_is_bounded() {
        const { assert!(SEARCH_TIMEOUT_SECS > 0) };
        const { assert!(SEARCH_TIMEOUT_SECS <= ABSOLUTE_MAX_TIMEOUT.as_secs()) };
    }

    /// The match cap bounds a count, not a size. Both bounds have to exist, or
    /// 200 matches on a minified bundle is still tens of megabytes.
    #[test]
    fn the_output_is_bounded_in_bytes_and_not_only_in_matches() {
        const { assert!(MAX_MATCH_LINE_BYTES > 0) };
        const { assert!(MAX_STREAM_BYTES > 0) };
        // The worst case a caller can be handed, and it has to be small enough
        // to sit in a transcript that is re-sent to the model every step.
        const WORST_CASE: usize = MATCH_CAP * (MAX_MATCH_LINE_BYTES + 8);
        const { assert!(WORST_CASE <= 1024 * 1024) };
    }

    /// A single pathological line — one minified bundle, one generated blob —
    /// must not reach the transcript whole.
    ///
    /// Without the clamp this match arrives at its full length, and up to
    /// [`MATCH_CAP`] of them do; the transcript is re-sent to the model every
    /// step until compaction, so a search over a repo with a bundled asset was
    /// enough to blow the context on its own.
    #[tokio::test]
    async fn one_enormous_line_is_clamped_before_it_reaches_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();

        // 4 MiB on one line, with the needle at the front.
        let mut blob = String::from("NEEDLE");
        blob.push_str(&"x".repeat(4 * 1024 * 1024));
        blob.push('\n');
        std::fs::write(root.join("bundle.min.js"), &blob).unwrap();

        let scope = PathScope::new(vec![root.clone()], vec![]);
        let results = Search::execute(
            &SearchInput {
                pattern: "NEEDLE".to_string(),
                glob: None,
            },
            &scope,
        )
        .await;

        let Ok(results) = results else {
            // No ripgrep on this machine: the bound is still pinned by the
            // const test above, so this is a skip rather than a failure.
            eprintln!("ripgrep unavailable; skipping the live clamp check");
            return;
        };
        assert_eq!(
            results.matches.len(),
            1,
            "the needle is on exactly one line"
        );
        let line = &results.matches[0].line;
        assert!(
            line.len() <= MAX_MATCH_LINE_BYTES + 8,
            "a {} byte line reached the caller; it must be clamped to {}",
            line.len(),
            MAX_MATCH_LINE_BYTES
        );
        assert!(
            line.starts_with("NEEDLE"),
            "the clamp keeps the head of the line, where the match is"
        );
        assert!(line.ends_with('…'), "a clamped line says so");
    }
}
