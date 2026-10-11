use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Remove terminal controls and bidirectional override characters from
/// producer text. Newlines are retained, CRLF is normalized, and tabs become
/// four spaces. The returned text is safe to pass to Ratatui without allowing
/// ANSI/OSC injection.
#[must_use]
pub fn sanitize_terminal_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    // ANSI control sequence introducer: its final byte is in
                    // the inclusive `@`..`~` range.
                    for control in chars.by_ref() {
                        if ('@'..='~').contains(&control) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // Operating System Command: terminated by BEL or ST
                    // (ESC backslash). Never allow title/hyperlink injection.
                    let mut escaped = false;
                    for control in chars.by_ref() {
                        if control == '\u{7}' || (escaped && control == '\\') {
                            break;
                        }
                        escaped = control == '\u{1b}';
                    }
                }
                Some(_) | None => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                }
                output.push('\n');
            }
            '\n' => output.push('\n'),
            '\t' => output.push_str("    "),
            // Directional marks, overrides, and isolates can visually reorder
            // trusted terminal chrome. Strip all of them rather than exposing
            // an invisible spoofing vector.
            '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => {}
            // C0/C1, DEL, CSI, OSC and their constituent controls.
            ch if ch.is_control() => {}
            _ => output.push(ch),
        }
    }
    output
}

#[must_use]
pub fn cell_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Unicode grapheme-aware wrapping by terminal cell width. Long tokens are
/// split only at grapheme boundaries; combining marks and emoji ZWJ sequences
/// remain intact.
#[must_use]
pub fn wrap_cells(input: &str, width: usize) -> Vec<String> {
    let clean = sanitize_terminal_text(input);
    if width == 0 {
        return Vec::new();
    }

    let mut result = Vec::new();
    for logical in clean.split('\n') {
        if logical.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0_usize;
        for token in logical.split_word_bounds() {
            let token_width = cell_width(token);
            if token_width == 0 {
                line.push_str(token);
                continue;
            }
            if line_width > 0 && line_width.saturating_add(token_width) > width {
                result.push(line.trim_end().to_owned());
                line.clear();
                line_width = 0;
                if token.trim().is_empty() {
                    continue;
                }
            }
            if token_width <= width.saturating_sub(line_width) {
                line.push_str(token);
                line_width += token_width;
                continue;
            }
            for grapheme in token.graphemes(true) {
                let grapheme_width = cell_width(grapheme);
                if line_width > 0 && line_width.saturating_add(grapheme_width) > width {
                    result.push(line.trim_end().to_owned());
                    line.clear();
                    line_width = 0;
                }
                if grapheme_width <= width {
                    line.push_str(grapheme);
                    line_width += grapheme_width;
                }
            }
        }
        result.push(line.trim_end().to_owned());
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Truncate to a cell width, adding an ellipsis when content was removed.
#[must_use]
pub fn truncate_cells(input: &str, width: usize) -> String {
    let clean = sanitize_terminal_text(input);
    if cell_width(&clean) <= width {
        return clean;
    }
    if width == 0 {
        return String::new();
    }
    let marker = if width >= 1 { "…" } else { "" };
    let available = width.saturating_sub(cell_width(marker));
    let mut output = String::new();
    let mut used = 0_usize;
    for grapheme in clean.graphemes(true) {
        let next = cell_width(grapheme);
        if used.saturating_add(next) > available {
            break;
        }
        output.push_str(grapheme);
        used += next;
    }
    output.push_str(marker);
    output
}

pub(crate) fn pad_cells(input: &str, width: usize) -> String {
    let mut output = truncate_cells(input, width);
    output.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(cell_width(&output)),
    ));
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_terminal_and_bidi_controls() {
        assert_eq!(
            sanitize_terminal_text("ok\u{1b}[31m red\u{7}\u{061c}\u{200e}\u{200f}\u{202e}x\r\ny"),
            "ok redx\ny"
        );
    }

    #[test]
    fn wrapping_obeys_cell_width_and_graphemes() {
        let lines = wrap_cells("alpha 界界 omega", 7);
        assert!(lines.iter().all(|line| cell_width(line) <= 7));
        assert_eq!(lines, ["alpha", "界界", "omega"]);
        let family = "👨‍👩‍👧‍👦";
        assert_eq!(wrap_cells(family, 2), [family]);
    }

    #[test]
    fn truncation_uses_cells_not_bytes() {
        assert_eq!(truncate_cells("ab界cd", 5), "ab界…");
        assert_eq!(pad_cells("界", 4), "界  ");
    }
}
