//! Tool-node namespace normalization and `with:` argument binding (Phase 5 T6).
//!
//! Two deterministic, model-free concerns a workflow **tool node** needs:
//!
//! * **Namespace normalization.** A manifest may spell a tool id with hyphens
//!   (`github.update-pull-request`), matching the workflow-id style, but the
//!   runtime tool registry and the knowledge built-in cards use underscores
//!   (`github.update_pull_request`). [`normalize_tool_name`] reconciles the two
//!   with a single rule — replace `-` with `_` — applied identically by the
//!   compile-time reference check ([`crate::compile::CompiledWorkflow::validate_references`])
//!   and the daemon's tool-node executor, so a manifest name that *compiles*
//!   against the registry also *resolves* at run time. It is an **alias, not a
//!   rename**: runtime names are never changed, and a manifest already written
//!   with underscores passes through unchanged (the rule is idempotent).
//!
//! * **Argument binding.** The compiled graph carries no per-node tool arguments,
//!   so a tool node binds them from the run's typed `inputs`. A step's optional
//!   `with:` map supplies them explicitly, its string values interpolated against
//!   the inputs with a single placeholder form — `${{ inputs.<name> }}` — and
//!   **no** expression language (exact-match substitution only). A value that is
//!   exactly one placeholder takes the input's *typed* JSON value (an integer
//!   stays an integer); a placeholder embedded in surrounding text substitutes
//!   the input stringified. An unknown input name is a binding failure; the
//!   compiler catches the statically-detectable case ahead of time
//!   ([`scan_input_refs`]), and the runtime catches any that slip through.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// Normalize a manifest tool id to the runtime/registry namespace by rewriting
/// `-` to `_`. See the module docs: an alias, not a rename, and idempotent (a
/// name already using underscores is returned unchanged).
#[must_use]
pub fn normalize_tool_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Locate every `${{ inputs.<name> }}` placeholder in `s`, returning each as
/// `(start, end_exclusive, input_name)` over `s`'s byte offsets. A malformed
/// placeholder — unterminated, or a non-`inputs.` reference (no expression
/// language is supported) — is an `Err` naming the offending span.
fn find_placeholders(s: &str) -> Result<Vec<(usize, usize, String)>, String> {
    const OPEN: &str = "${{";
    const CLOSE: &str = "}}";
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = s[cursor..].find(OPEN) {
        let start = cursor + rel;
        let after_open = start + OPEN.len();
        let close_rel = s[after_open..]
            .find(CLOSE)
            .ok_or_else(|| format!("unterminated `${{{{` placeholder in `{s}`"))?;
        let inner_end = after_open + close_rel;
        let end = inner_end + CLOSE.len();
        let inner = s[after_open..inner_end].trim();
        let name = inner.strip_prefix("inputs.").ok_or_else(|| {
            format!(
                "`{}` is not a supported placeholder (only `${{{{ inputs.<name> }}}}`)",
                &s[start..end]
            )
        })?;
        if name.is_empty() {
            return Err(format!("empty input name in `{}`", &s[start..end]));
        }
        out.push((start, end, name.to_string()));
        cursor = end;
    }
    Ok(out)
}

/// The input names a single string value references through `${{ inputs.<name> }}`
/// placeholders, in order (duplicates preserved). An `Err` names a malformed
/// placeholder. The compiler uses this to reject a `with:` value that references
/// an input the workflow never declares, before any run starts.
pub fn scan_input_refs(s: &str) -> Result<Vec<String>, String> {
    Ok(find_placeholders(s)?
        .into_iter()
        .map(|(_, _, name)| name)
        .collect())
}

/// Render a JSON value as the plain string an embedded placeholder substitutes:
/// a string as itself (unquoted), everything else via its JSON encoding (so a
/// number interpolates as its digits, not `"7"`).
fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Interpolate one `with:` value against `inputs`. A non-string value passes
/// through unchanged; a string is substituted per the module docs (whole-string
/// placeholder → typed passthrough; embedded placeholders → stringified).
fn interpolate(value: &Value, inputs: &Value) -> Result<Value, String> {
    let Value::String(s) = value else {
        return Ok(value.clone());
    };
    let placeholders = find_placeholders(s)?;
    if placeholders.is_empty() {
        return Ok(Value::String(s.clone()));
    }
    // A value that is exactly one placeholder takes the input's typed value.
    if let [(start, end, name)] = placeholders.as_slice() {
        if *start == 0 && *end == s.len() {
            return lookup(inputs, name).cloned();
        }
    }
    // Otherwise splice each placeholder's stringified value into the surrounding
    // text.
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (start, end, name) in &placeholders {
        out.push_str(&s[last..*start]);
        out.push_str(&stringify(lookup(inputs, name)?));
        last = *end;
    }
    out.push_str(&s[last..]);
    Ok(Value::String(out))
}

/// Look up `name` in the run's typed inputs (a JSON object), or a legible error
/// naming the missing input.
fn lookup<'a>(inputs: &'a Value, name: &str) -> Result<&'a Value, String> {
    inputs
        .get(name)
        .ok_or_else(|| format!("unknown workflow input `{name}`"))
}

/// Bind a tool node's `with:` map against the run's typed `inputs`, substituting
/// `${{ inputs.<name> }}` placeholders (see the module docs). Returns the bound
/// argument object (a JSON object), or the first offending detail — a missing
/// input or a malformed placeholder — which the executor surfaces as a legible
/// `workflow.tool-binding-missing` node failure.
pub fn bind_with(with: &BTreeMap<String, Value>, inputs: &Value) -> Result<Value, String> {
    let mut bound = Map::with_capacity(with.len());
    for (key, value) in with {
        bound.insert(key.clone(), interpolate(value, inputs)?);
    }
    Ok(Value::Object(bound))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalization_is_a_hyphen_to_underscore_alias() {
        assert_eq!(
            normalize_tool_name("github.update-pull-request"),
            "github.update_pull_request"
        );
        // Idempotent: an already-underscored name is unchanged.
        assert_eq!(
            normalize_tool_name("github.update_pull_request"),
            "github.update_pull_request"
        );
        // A name with no hyphens is unchanged.
        assert_eq!(normalize_tool_name("repository.test"), "repository.test");
    }

    #[test]
    fn scan_reports_referenced_inputs_and_rejects_malformed() {
        assert_eq!(
            scan_input_refs("${{ inputs.pull_request }}").unwrap(),
            vec!["pull_request".to_string()]
        );
        assert_eq!(
            scan_input_refs("PR #${{ inputs.pull_request }} on ${{ inputs.repo }}").unwrap(),
            vec!["pull_request".to_string(), "repo".to_string()]
        );
        // A non-inputs placeholder (no expression language) is rejected.
        assert!(scan_input_refs("${{ env.SECRET }}").is_err());
        // An unterminated placeholder is rejected.
        assert!(scan_input_refs("${{ inputs.x").is_err());
        // Plain text references nothing.
        assert!(scan_input_refs("just text").unwrap().is_empty());
    }

    #[test]
    fn whole_string_placeholder_preserves_the_input_type() {
        let inputs = json!({ "pull_request": 7, "flag": true });
        let with: BTreeMap<String, Value> = [
            ("number".to_string(), json!("${{ inputs.pull_request }}")),
            ("enabled".to_string(), json!("${{ inputs.flag }}")),
        ]
        .into_iter()
        .collect();
        let bound = bind_with(&with, &inputs).unwrap();
        // An integer input stays an integer, a bool stays a bool — not stringified.
        assert_eq!(bound["number"], json!(7));
        assert_eq!(bound["enabled"], json!(true));
    }

    #[test]
    fn embedded_placeholder_substitutes_stringified() {
        let inputs = json!({ "pull_request": 7 });
        let with: BTreeMap<String, Value> = [(
            "body".to_string(),
            json!("fixes PR #${{ inputs.pull_request }}"),
        )]
        .into_iter()
        .collect();
        let bound = bind_with(&with, &inputs).unwrap();
        assert_eq!(bound["body"], json!("fixes PR #7"));
    }

    #[test]
    fn non_string_values_pass_through() {
        let inputs = json!({});
        let with: BTreeMap<String, Value> = [
            ("count".to_string(), json!(3)),
            ("nested".to_string(), json!({ "a": 1 })),
        ]
        .into_iter()
        .collect();
        let bound = bind_with(&with, &inputs).unwrap();
        assert_eq!(bound["count"], json!(3));
        assert_eq!(bound["nested"], json!({ "a": 1 }));
    }

    #[test]
    fn an_unknown_input_is_a_legible_binding_error() {
        let inputs = json!({ "pull_request": 7 });
        let with: BTreeMap<String, Value> = [("x".to_string(), json!("${{ inputs.missing }}"))]
            .into_iter()
            .collect();
        let err = bind_with(&with, &inputs).unwrap_err();
        assert!(err.contains("missing"), "error names the input: {err}");
    }
}
