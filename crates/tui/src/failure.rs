//! Bounded diagnostic projection. Rules and exact-output vectors are shared
//! with the desktop in testdata/failure-sanitization.json. Never log raw input.
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rules {
    max_input: usize,
    max_output: usize,
    keys: Vec<String>,
    prefixes: Vec<String>,
}
const CONTRACT: &str = include_str!("../../../testdata/failure-sanitization.json");
fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| serde_json::from_str(CONTRACT).expect("embedded failure contract"))
}
fn name(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}
fn quote_end(chars: &[char], start: usize) -> usize {
    let mut i = start + 1;
    while i < chars.len() && chars[i] != '\n' {
        if chars[i] == '\\' {
            i += 1;
        } else if chars[i] == chars[start] {
            return i + 1;
        }
        i += 1;
    }
    i.min(chars.len())
}
fn spaces(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && matches!(chars[i], ' ' | '\t') {
        i += 1;
    }
    i
}
fn value_end(chars: &[char], start: usize, whole_line: bool) -> usize {
    if start >= chars.len() {
        return start;
    }
    if !whole_line && matches!(chars[start], '"' | '\'') {
        return quote_end(chars, start);
    }
    let mut depth: usize = 0;
    let mut i = start;
    while i < chars.len() && chars[i] != '\n' {
        if !whole_line {
            match chars[i] {
                '"' | '\'' if depth > 0 => {
                    i = quote_end(chars, i);
                    continue;
                }
                '{' | '[' => depth += 1,
                '}' | ']' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                c if depth == 0
                    && (c.is_whitespace()
                        || matches!(c, '&' | ',' | '}' | ']' | '"' | '\'' | ';' | '#')) =>
                {
                    break
                }
                _ => {}
            }
        }
        i += 1;
    }
    i
}

#[must_use]
pub fn sanitize_failure_text(raw: &str) -> String {
    let rules = rules();
    // Bound work before allocation, including hostile streams of controls.
    let input: Vec<char> = raw.chars().take(rules.max_input + 1).collect();
    let truncated = input.len() > rules.max_input || input.len() > rules.max_output;
    let mut chars = Vec::new();
    let mut i = 0;
    while i < input.len().min(rules.max_input) && chars.len() < rules.max_output {
        let c = input[i];
        if c == '\u{1b}' && input.get(i + 1) == Some(&'[') {
            i += 2;
            while i < input.len() {
                let end = ('\u{40}'..='\u{7e}').contains(&input[i]);
                i += 1;
                if end {
                    break;
                }
            }
            continue;
        }
        if c == '\u{1b}' && input.get(i + 1) == Some(&']') {
            i += 2;
            while i < input.len() {
                if input[i] == '\u{7}' {
                    i += 1;
                    break;
                }
                if input[i] == '\u{1b}' && input.get(i + 1) == Some(&'\\') {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if matches!(c, '\n' | '\t')
            || (!c.is_control()
                && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        {
            chars.push(c);
        }
        i += 1;
    }
    let mut safe = String::new();
    i = 0;
    while i < chars.len() {
        // JSON keys are decoded independently, so escaped names are credentials
        // too. Each value ends at its own delimiter, preserving adjacent context.
        if chars[i] == '"' {
            let end = quote_end(&chars, i);
            let colon = spaces(&chars, end);
            if chars.get(colon) == Some(&':') {
                let key: String = chars[i..end].iter().collect();
                if serde_json::from_str::<String>(&key)
                    .is_ok_and(|key| rules.keys.contains(&key.to_ascii_lowercase()))
                {
                    let start = spaces(&chars, colon + 1);
                    safe.extend(&chars[i..start]);
                    let end = value_end(&chars, start, false);
                    if chars.get(start) == Some(&'"') {
                        safe.push_str("\"[REDACTED]");
                        if end > start + 1 && chars.get(end - 1) == Some(&'"') {
                            safe.push('"');
                        }
                    } else {
                        safe.push_str("[REDACTED]");
                    }
                    i = end;
                    continue;
                }
            }
        }
        if name(chars[i]) && (i == 0 || !name(chars[i - 1])) {
            let mut end = i;
            while end < chars.len() && name(chars[end]) {
                end += 1;
            }
            let key: String = chars[i..end]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase();
            if rules.prefixes.iter().any(|prefix| key.starts_with(prefix)) {
                safe.push_str("[REDACTED]");
                i = value_end(&chars, i, false);
                continue;
            }
            let separator = spaces(&chars, end);
            let delimiter = chars.get(separator).copied();
            let explicit = matches!(delimiter, Some(':' | '='));
            let label = matches!(
                key.as_str(),
                "authorization" | "bearer" | "token" | "api_key" | "apikey" | "password" | "secret"
            );
            if rules.keys.contains(&key) && (explicit || (label && separator > end)) {
                let start = spaces(&chars, separator + usize::from(explicit));
                safe.extend(&chars[i..start]);
                let whole_line = delimiter != Some('=')
                    && matches!(
                        key.as_str(),
                        "authorization"
                            | "proxy-authorization"
                            | "password"
                            | "secret"
                            | "cookie"
                            | "set-cookie"
                    );
                safe.push_str("[REDACTED]");
                i = value_end(&chars, start, whole_line);
                continue;
            }
        }
        safe.push(chars[i]);
        i += 1;
    }
    let flat = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat
        .chars()
        .take(rules.max_output - usize::from(truncated))
        .collect();
    if truncated {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_desktop_and_terminal_contract() {
        let contract: serde_json::Value = serde_json::from_str(CONTRACT).unwrap();
        for case in contract["cases"].as_array().unwrap() {
            assert_eq!(
                sanitize_failure_text(case["raw"].as_str().unwrap()),
                case["safe"].as_str().unwrap()
            );
        }
    }
}
