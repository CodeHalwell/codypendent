//! Observation compaction, Level 1 (Chapter 09).
//!
//! Large command output never enters model context whole. It is compacted to a
//! [`SalientView`]: the command, its exit code and duration, and — per stream —
//! the first and last lines, any error-matching lines, and the [`ArtifactRef`]
//! of the full output. The model reads this; if it needs more it rehydrates from
//! the artifact.

use codypendent_protocol::ArtifactRef;

use super::IN_MEMORY_CAP;

/// Lines kept from the head of a stream.
const SALIENT_HEAD: usize = 40;
/// Lines kept from the tail of a stream.
const SALIENT_TAIL: usize = 40;
/// A single salient line is clamped to this many bytes so one pathological line
/// cannot bloat the compacted view.
const SALIENT_MAX_LINE_LEN: usize = 2048;
/// Case-insensitive substrings that mark a line as salient regardless of its
/// position (Chapter 09 / STEP 1.7 rule 4).
const ERROR_MARKERS: [&str; 5] = ["error", "warning", "panic", "failed", "fatal"];

/// The compacted, model-facing view of one command execution.
#[derive(Debug, Clone)]
pub struct SalientView {
    /// The command as `program arg arg …`.
    pub command: String,
    /// Process exit code, or `None` if the process was killed (e.g. on timeout).
    pub exit_code: Option<i32>,
    /// Whether the command was killed for exceeding its timeout.
    pub timed_out: bool,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u128,
    /// Compacted standard output.
    pub stdout: SalientStream,
    /// Compacted standard error.
    pub stderr: SalientStream,
}

/// The compacted view of a single output stream.
#[derive(Debug, Clone)]
pub struct SalientStream {
    /// Head + tail + error-matching lines, in original order, with
    /// `… N lines omitted …` markers where the selection is not contiguous.
    pub lines: Vec<String>,
    /// Total number of lines in the captured output.
    pub total_lines: usize,
    /// Bytes captured (== full length unless `overflowed`).
    pub captured_bytes: usize,
    /// Whether output exceeded [`MAX_CAPTURE_BYTES`](super::MAX_CAPTURE_BYTES)
    /// and the tail was dropped from capture.
    pub overflowed: bool,
    /// Whether any lines were omitted from `lines` (i.e. the model must consult
    /// the artifact to see everything).
    pub truncated: bool,
    /// Whether the captured output exceeded the 1 MiB in-memory soft cap.
    pub large: bool,
    /// The full captured output, if it was spilled to the store.
    pub artifact: Option<ArtifactRef>,
}

impl SalientStream {
    /// An empty stream (no output produced).
    pub fn empty() -> Self {
        Self {
            lines: Vec::new(),
            total_lines: 0,
            captured_bytes: 0,
            overflowed: false,
            truncated: false,
            large: false,
            artifact: None,
        }
    }
}

/// Whether a line contains any error marker (case-insensitive).
fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ERROR_MARKERS.iter().any(|m| lower.contains(m))
}

/// Clamp a single line to [`SALIENT_MAX_LINE_LEN`] bytes on a char boundary,
/// appending an ellipsis when truncated.
fn clamp_line(line: &str) -> String {
    if line.len() <= SALIENT_MAX_LINE_LEN {
        return line.to_string();
    }
    let mut end = SALIENT_MAX_LINE_LEN;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

/// Build the compacted [`SalientStream`] for `bytes`. `overflowed` marks that
/// capture hit the hard ceiling; `artifact` is the reference to the full output
/// (present whenever it was spilled).
pub(crate) fn compute_stream(
    bytes: &[u8],
    overflowed: bool,
    artifact: Option<ArtifactRef>,
) -> SalientStream {
    let captured_bytes = bytes.len();
    if bytes.is_empty() {
        return SalientStream {
            artifact,
            ..SalientStream::empty()
        };
    }
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    // Select indices: head, tail, and every error-matching line.
    let mut selected: Vec<usize> = Vec::new();
    let head_end = SALIENT_HEAD.min(total_lines);
    selected.extend(0..head_end);
    let tail_start = total_lines.saturating_sub(SALIENT_TAIL);
    selected.extend(tail_start..total_lines);
    for (i, line) in lines.iter().enumerate() {
        if is_error_line(line) {
            selected.push(i);
        }
    }
    selected.sort_unstable();
    selected.dedup();

    // Emit selected lines with omission markers across gaps.
    let mut out: Vec<String> = Vec::with_capacity(selected.len() + 4);
    let mut prev: Option<usize> = None;
    for &idx in &selected {
        if let Some(p) = prev {
            if idx > p + 1 {
                out.push(format!("… {} lines omitted …", idx - p - 1));
            }
        }
        out.push(clamp_line(lines[idx]));
        prev = Some(idx);
    }

    SalientStream {
        lines: out,
        total_lines,
        captured_bytes,
        overflowed,
        truncated: selected.len() < total_lines,
        large: captured_bytes > IN_MEMORY_CAP,
        artifact,
    }
}

impl SalientView {
    /// Render the compacted view as the plain-text block that enters model
    /// context: the command, its result, and each non-empty stream's salient
    /// lines with a reference to the full artifact.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("$ {}\n", self.command));
        match (self.exit_code, self.timed_out) {
            (_, true) => s.push_str(&format!("killed after timeout ({} ms)\n", self.duration_ms)),
            (Some(code), false) => {
                s.push_str(&format!("exit {} ({} ms)\n", code, self.duration_ms))
            }
            (None, false) => s.push_str(&format!("killed by signal ({} ms)\n", self.duration_ms)),
        }
        render_stream(&mut s, "stdout", &self.stdout);
        render_stream(&mut s, "stderr", &self.stderr);
        s
    }
}

fn render_stream(s: &mut String, name: &str, stream: &SalientStream) {
    if stream.total_lines == 0 {
        s.push_str(&format!("--- {name}: empty ---\n"));
        return;
    }
    let art = stream
        .artifact
        .as_ref()
        .map(|a| {
            format!(
                ", artifact {} sha256:{}",
                a.id,
                &a.sha256[..a.sha256.len().min(12)]
            )
        })
        .unwrap_or_default();
    s.push_str(&format!(
        "--- {name}: {} lines, {} bytes{}{}{} ---\n",
        stream.total_lines,
        stream.captured_bytes,
        if stream.truncated { " (truncated)" } else { "" },
        if stream.overflowed {
            " (capture overflowed)"
        } else {
            ""
        },
        art,
    ));
    for line in &stream.lines {
        s.push_str(line);
        s.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_is_not_truncated() {
        let text = "line 1\nline 2\nline 3\n";
        let stream = compute_stream(text.as_bytes(), false, None);
        assert_eq!(stream.total_lines, 3);
        assert!(!stream.truncated);
        assert_eq!(stream.lines, vec!["line 1", "line 2", "line 3"]);
        assert!(!stream.large);
    }

    #[test]
    fn large_output_keeps_head_tail_and_error_lines() {
        let mut text = String::new();
        for i in 0..500 {
            if i == 250 {
                text.push_str("this line has an ERROR in the middle\n");
            } else {
                text.push_str(&format!("line {i}\n"));
            }
        }
        let stream = compute_stream(text.as_bytes(), false, None);
        assert_eq!(stream.total_lines, 500);
        assert!(stream.truncated);
        // Head, tail and the error line survive.
        assert!(stream.lines.iter().any(|l| l == "line 0"));
        assert!(stream.lines.iter().any(|l| l == "line 499"));
        assert!(stream
            .lines
            .iter()
            .any(|l| l.contains("ERROR in the middle")));
        // A gap marker appears.
        assert!(stream.lines.iter().any(|l| l.contains("lines omitted")));
        // Far fewer than 500 lines are retained.
        assert!(stream.lines.len() < 200);
    }

    #[test]
    fn error_markers_are_case_insensitive() {
        assert!(is_error_line("build FAILED with 3 problems"));
        assert!(is_error_line("warning: unused variable"));
        assert!(is_error_line("thread 'main' panicked"));
        assert!(!is_error_line("all good here"));
    }

    #[test]
    fn empty_output_reports_empty() {
        let stream = compute_stream(b"", false, None);
        assert_eq!(stream.total_lines, 0);
        assert!(stream.lines.is_empty());
    }
}
