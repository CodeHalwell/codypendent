//! Deriving a short, human-readable display label for a tool call.
//!
//! The `ToolStarted` wire event used to carry only a `args_digest` (a
//! SHA-256 hash of the tool arguments) — never anything a human could read,
//! so a tool card rendered as a bare `workspace.read_file ✓` with no way to
//! tell *which* file. [`tool_label`] is called at the same site that hashes
//! the arguments (`crates/runtime/src/agent.rs`'s `run_tool`), while the raw
//! `args` are still in scope, and produces a short display string threaded
//! onto `ToolStarted::label` — so a client can render `workspace.read_file ·
//! services/main.py` instead.
//!
//! This is a DISPLAY STRING ONLY: never the full arguments, never file
//! contents. `git.apply_patch`'s only payload is the full patch body (a
//! blob of file content) plus a `cwd` — that is the clearest example of an
//! argument that must never surface here, and [`tool_label`] never returns
//! it. The derivation is conservative by construction: an unrecognized tool,
//! or a recognized tool missing its expected argument, yields `None` — the
//! tool card then renders exactly as it did before this field existed.

use serde_json::Value;

use super::{ApplyPatch, ArtifactRead, EditFile, ReadFile, Search, Shell, WebSearch};

/// Hard ceiling on a derived label's length, in `char`s. Longer values are
/// truncated with a trailing `…`. Short enough to sit on one line next to the
/// tool name and status glyph without wrapping a typical terminal width, and
/// far below anything that could meaningfully blow up the event.
const MAX_LABEL_CHARS: usize = 80;

/// Derive a short display label for `tool`'s call from its raw JSON `args` —
/// the same `args` value about to be hashed into `ToolStarted.args_digest`,
/// read here before that hash discards them. Pure and total: never panics,
/// never splits a multi-byte character, never returns the full argument blob.
///
/// Recognized tools (matched on the same stable `NAME` constants
/// [`ReadFile::NAME`]/[`Search::NAME`]/[`Shell::NAME`]/[`ApplyPatch::NAME`]
/// the agent loop's `prepare` dispatches on, plus a couple of generic aliases
/// in case a future tool reuses them):
///
/// * `workspace.read_file` (and `read_file`/`workspace.write_file`/
///   `write_file`/`git.apply_patch`/`apply_patch` — any write-ish tool that
///   might carry one) → its `path` (or `file`/`filename`) argument.
/// * `workspace.edit_file` (and `edit_file`) → its `path` argument — the
///   file being edited, not the `edits` array of search/replace pairs (that
///   carries file content and must never surface here, same as
///   `git.apply_patch`'s patch body).
/// * `shell.run` → the command it runs: the structured `program` + `args`
///   array Chapter 11 requires (never an unparsed shell string), joined the
///   way a human reads a command line — falling back to a flat `command`/
///   `cmd` string arg if that is what is present instead.
/// * `workspace.search` (and `search`) → its `query` (or `pattern`) argument.
/// * `mcp.<server>.<tool>` → the `server.tool` pair itself: the args schema is
///   server-defined, so no argument is known-safe to surface.
/// * anything else → `None`, conservatively — no guessing at some other
///   argument that might not be safe to show.
pub fn tool_label(tool: &str, args: &Value) -> Option<String> {
    let raw = match tool {
        ReadFile::NAME | "read_file" => string_arg(args, &["path", "file", "filename"]),
        // `git.apply_patch`'s only payload is the full patch body (file
        // content) plus a `cwd` — there is no short path-like arg to show
        // today, and the patch text itself must NEVER surface as a label.
        // Matched here anyway (and against the task-facing alias
        // `apply_patch`) so a hypothetical future write tool that does carry
        // a `path` picks one up for free; today this arm simply yields
        // `None` for `git.apply_patch`'s real argument shape.
        "workspace.write_file" | "write_file" | ApplyPatch::NAME | "apply_patch" => {
            string_arg(args, &["path", "file", "filename"])
        }
        // `workspace.edit_file`'s `edits` array carries the search/replace
        // text (file content) — only `path` is safe to show, mirroring
        // `git.apply_patch`'s patch-body exclusion above.
        EditFile::NAME | "edit_file" => string_arg(args, &["path", "file", "filename"]),
        Shell::NAME => shell_command_label(args),
        Search::NAME | "search" => string_arg(args, &["query", "pattern"]),
        // `artifact.read`'s only argument is the artifact id — an opaque UUID,
        // safe to show and exactly what identifies the call.
        ArtifactRead::NAME => string_arg(args, &["id"]),
        // `web.search` (PR C1): the query is the label, like `workspace.search`.
        WebSearch::NAME => string_arg(args, &["query"]),
        // MCP client (PR B): an `mcp.<server>.<tool>` args schema is
        // server-defined, so no argument is known-safe to surface — the
        // server.tool pair itself (already part of the tool name, and how the
        // operator declared it) is the only display information.
        name if name.starts_with("mcp.") => Some(format!("MCP {}", &name["mcp.".len()..])),
        _ => None,
    }?;
    let sanitized = sanitize_label(&raw);
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// The first of `keys` present in `args` as a non-empty JSON string.
fn string_arg(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

/// `shell.run`'s command is not a single string arg: Chapter 11 requires a
/// structured request — a `program` string plus an `args` array, never an
/// unparsed shell string (see `parse_command_request` in
/// `crates/runtime/src/agent.rs`). Render it the way a human reads a command
/// line: `program arg1 arg2 ...`. Falls back to a flat `command`/`cmd`
/// string arg if `program` is absent (defensive: not the shape this tool's
/// own parser accepts, but harmless to recognize if some caller sends it).
fn shell_command_label(args: &Value) -> Option<String> {
    if let Some(program) = args.get("program").and_then(Value::as_str) {
        let mut command = program.to_string();
        if let Some(items) = args.get("args").and_then(Value::as_array) {
            for item in items.iter().filter_map(Value::as_str) {
                command.push(' ');
                command.push_str(item);
            }
        }
        return Some(command);
    }
    string_arg(args, &["command", "cmd"])
}

/// Collapse to a single line — an embedded newline must never break the tool
/// card out of its one-line summary — and cap the length. This is the bound
/// that keeps a label a DISPLAY string, never the raw arguments: it can never
/// grow the event or leak a large blob.
fn sanitize_label(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, MAX_LABEL_CHARS)
}

/// Truncate to at most `max` `char`s (never splitting a multi-byte
/// character), appending `…` when truncation actually occurred. The result
/// is always `<= max` chars, ellipsis included.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut indices = s.char_indices();
    if let Some((trunc_idx, _)) = indices.nth(max.saturating_sub(1)) {
        if indices.next().is_some() {
            return format!("{}…", &s[..trunc_idx]);
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_file_label_is_the_path() {
        assert_eq!(
            tool_label("workspace.read_file", &json!({"path": "services/main.py"})),
            Some("services/main.py".to_string())
        );
    }

    #[test]
    fn read_file_falls_back_to_file_then_filename() {
        assert_eq!(
            tool_label("workspace.read_file", &json!({"file": "a.rs"})),
            Some("a.rs".to_string())
        );
        assert_eq!(
            tool_label("workspace.read_file", &json!({"filename": "b.rs"})),
            Some("b.rs".to_string())
        );
    }

    #[test]
    fn read_file_alias_name_is_recognized() {
        assert_eq!(
            tool_label("read_file", &json!({"path": "c.rs"})),
            Some("c.rs".to_string())
        );
    }

    #[test]
    fn shell_run_label_joins_program_and_args() {
        assert_eq!(
            tool_label("shell.run", &json!({"program": "cargo", "args": ["test"]})),
            Some("cargo test".to_string())
        );
    }

    #[test]
    fn shell_run_label_with_multiple_args_joins_all() {
        assert_eq!(
            tool_label(
                "shell.run",
                &json!({"program": "cargo", "args": ["test", "--all-features"]})
            ),
            Some("cargo test --all-features".to_string())
        );
    }

    #[test]
    fn shell_run_label_with_no_args_is_just_the_program() {
        assert_eq!(
            tool_label("shell.run", &json!({"program": "ls", "args": []})),
            Some("ls".to_string())
        );
    }

    #[test]
    fn shell_run_falls_back_to_a_flat_command_string() {
        assert_eq!(
            tool_label("shell.run", &json!({"command": "cargo test"})),
            Some("cargo test".to_string())
        );
    }

    #[test]
    fn search_label_is_the_pattern() {
        assert_eq!(
            tool_label("workspace.search", &json!({"pattern": "TODO"})),
            Some("TODO".to_string())
        );
    }

    #[test]
    fn search_falls_back_to_query() {
        assert_eq!(
            tool_label("workspace.search", &json!({"query": "TODO"})),
            Some("TODO".to_string())
        );
    }

    #[test]
    fn web_search_label_is_the_query() {
        assert_eq!(
            tool_label(
                "web.search",
                &json!({"query": "rust async", "max_results": 5})
            ),
            Some("rust async".to_string())
        );
        assert_eq!(tool_label("web.search", &json!({})), None);
    }

    #[test]
    fn search_alias_name_is_recognized() {
        assert_eq!(
            tool_label("search", &json!({"pattern": "TODO"})),
            Some("TODO".to_string())
        );
    }

    #[test]
    fn write_file_label_is_the_path() {
        assert_eq!(
            tool_label("workspace.write_file", &json!({"path": "services/main.py"})),
            Some("services/main.py".to_string())
        );
    }

    #[test]
    fn write_file_alias_name_is_recognized() {
        assert_eq!(
            tool_label("write_file", &json!({"path": "new_file.rs"})),
            Some("new_file.rs".to_string())
        );
    }

    #[test]
    fn edit_file_label_is_the_path_not_the_edits() {
        assert_eq!(
            tool_label(
                "workspace.edit_file",
                &json!({
                    "path": "services/main.py",
                    "edits": [{"search": "old", "replace": "new"}],
                })
            ),
            Some("services/main.py".to_string())
        );
    }

    #[test]
    fn edit_file_alias_name_is_recognized() {
        assert_eq!(
            tool_label(
                "edit_file",
                &json!({"path": "b.rs", "edits": [{"search": "x", "replace": "y"}]})
            ),
            Some("b.rs".to_string())
        );
    }

    #[test]
    fn edit_file_never_leaks_the_edits_array() {
        // No `path` present — only the (content-bearing) `edits` array. Must
        // yield `None`, never fall through to stringify `edits` somehow.
        assert_eq!(
            tool_label(
                "workspace.edit_file",
                &json!({"edits": [{"search": "old", "replace": "new"}]})
            ),
            None
        );
    }

    /// The clearest safety case: `git.apply_patch`'s only payload is the full
    /// patch text (file content) plus a `cwd` — it must never leak through as
    /// a label, and there is no path-like arg to fall back to either.
    #[test]
    fn apply_patch_never_leaks_the_patch_body() {
        assert_eq!(
            tool_label(
                "git.apply_patch",
                &json!({"patch": "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new", "cwd": "/repo"})
            ),
            None
        );
    }

    #[test]
    fn mcp_label_is_the_server_tool_pair() {
        assert_eq!(
            tool_label("mcp.github.create_issue", &json!({"title": "secret?"})),
            Some("MCP github.create_issue".to_string())
        );
    }

    #[test]
    fn unrecognized_tool_yields_none() {
        assert_eq!(
            tool_label("github.get_pull_request", &json!({"number": 42})),
            None
        );
        assert_eq!(tool_label("git.diff", &json!({})), None);
        assert_eq!(
            tool_label("blackboard.post", &json!({"kind": "finding"})),
            None
        );
    }

    #[test]
    fn missing_expected_arg_yields_none() {
        assert_eq!(tool_label("workspace.read_file", &json!({})), None);
        assert_eq!(tool_label("shell.run", &json!({})), None);
        assert_eq!(tool_label("workspace.search", &json!({})), None);
    }

    #[test]
    fn empty_string_arg_yields_none() {
        assert_eq!(
            tool_label("workspace.read_file", &json!({"path": ""})),
            None
        );
    }

    #[test]
    fn non_string_arg_yields_none() {
        assert_eq!(
            tool_label("workspace.read_file", &json!({"path": 42})),
            None
        );
    }

    #[test]
    fn label_is_truncated_with_a_trailing_ellipsis() {
        let long_path = format!("{}/file.rs", "a/".repeat(60)); // well over 80 chars
        let label = tool_label("workspace.read_file", &json!({"path": long_path})).unwrap();
        assert_eq!(label.chars().count(), MAX_LABEL_CHARS);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn label_within_the_limit_is_unchanged() {
        let path = "src/lib.rs";
        assert_eq!(
            tool_label("workspace.read_file", &json!({"path": path})),
            Some(path.to_string())
        );
    }

    #[test]
    fn label_at_exactly_the_limit_is_unchanged() {
        let path = "a".repeat(MAX_LABEL_CHARS);
        let label = tool_label("workspace.read_file", &json!({"path": path.clone()})).unwrap();
        assert_eq!(label, path);
        assert!(!label.ends_with('…'));
    }

    #[test]
    fn embedded_newlines_collapse_to_a_single_line() {
        let label = tool_label(
            "workspace.search",
            &json!({"pattern": "line one\nline two"}),
        )
        .unwrap();
        assert_eq!(label, "line one line two");
        assert!(!label.contains('\n'));
    }

    #[test]
    fn embedded_tabs_and_repeated_whitespace_collapse_too() {
        let label = tool_label("workspace.read_file", &json!({"path": "a/b\t\t  c.rs"})).unwrap();
        assert_eq!(label, "a/b c.rs");
    }
}
