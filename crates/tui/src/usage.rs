//! Pure measured-usage formatting shared by terminal clients.

/// Qualify only dimensions that are actually displayed. Missing amounts stay
/// absent; missing historical coverage must never imply an exact total.
pub fn coverage_label(
    coverage: Option<codypendent_protocol::UsageCoverage>,
    dimensions: [bool; 3],
) -> Option<&'static str> {
    let Some(coverage) = coverage else {
        return Some("coverage unknown");
    };
    let measured = [
        coverage.prompt_requests,
        coverage.completion_requests,
        coverage.cost_requests,
    ];
    (coverage.requests == 0
        || coverage.unmeasured_retries > 0
        || dimensions
            .into_iter()
            .zip(measured)
            .any(|(shown, count)| shown && count != coverage.requests))
    .then_some("partial")
}

/// A MEASURED cost, in USD millionths (`EventBody::RunUsage.cost_micros`).
///
/// Four decimals, matching `crates/cli/src/commands.rs`'s `render_cost` — a run
/// against a cheap model costs a fraction of a cent, and a two-decimal figure
/// would report almost every one of them as `$0.00`.
pub fn format_cost_micros(micros: u64) -> String {
    if (1..100).contains(&micros) {
        return format!("$0.{micros:06}");
    }
    format!("${}.{:04}", micros / 1_000_000, (micros % 1_000_000) / 100)
}

/// `1234` → `1,234`. Token counts are the one place in the shell where the
/// magnitude is the message; unseparated digits make 10000 and 100000 read
/// alike at a glance.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The measured-usage chip: `1,234 in · 567 out · $0.0034`.
///
/// Only measured dimensions appear. An absent one means the provider did not
/// report it, never zero — so a run with tokens and no price (every unpriced
/// local model) shows its tokens and no money, rather than a dash that reads as
/// "this run was free". `None` here means nothing at all was measured.
pub fn format_usage(
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    cost_micros: Option<u64>,
    coverage: Option<codypendent_protocol::UsageCoverage>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(tokens) = prompt_tokens {
        parts.push(format!("{} in", thousands(tokens)));
    }
    if let Some(tokens) = completion_tokens {
        parts.push(format!("{} out", thousands(tokens)));
    }
    if let Some(micros) = cost_micros {
        parts.push(format_cost_micros(micros));
    }
    if !parts.is_empty() {
        if let Some(label) = coverage_label(
            coverage,
            [
                prompt_tokens.is_some(),
                completion_tokens.is_some(),
                cost_micros.is_some(),
            ],
        ) {
            parts.push(label.to_string());
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::UsageCoverage;

    #[test]
    fn usage_distinguishes_unknown_partial_complete_zero_and_unmeasured() {
        let complete = UsageCoverage {
            requests: 2,
            prompt_requests: 2,
            completion_requests: 2,
            cost_requests: 2,
            unmeasured_retries: 0,
        };
        assert_eq!(format_usage(None, None, None, None), None);
        assert_eq!(
            format_usage(Some(0), None, None, None).as_deref(),
            Some("0 in · coverage unknown")
        );
        assert_eq!(
            format_usage(Some(1200), Some(0), Some(11), Some(complete)).as_deref(),
            Some("1,200 in · 0 out · $0.000011")
        );
        assert_eq!(
            format_usage(None, None, Some(0), Some(complete)).as_deref(),
            Some("$0.0000")
        );
        for partial in [
            UsageCoverage {
                cost_requests: 1,
                ..complete
            },
            UsageCoverage {
                unmeasured_retries: 1,
                ..complete
            },
        ] {
            assert_eq!(
                format_usage(None, None, Some(11), Some(partial)).as_deref(),
                Some("$0.000011 · partial")
            );
        }
        // An absent dimension does not imply zero and does not disqualify a
        // complete measurement of the dimension the provider did report.
        assert_eq!(
            format_usage(
                Some(1200),
                None,
                None,
                Some(UsageCoverage {
                    cost_requests: 0,
                    ..complete
                })
            )
            .as_deref(),
            Some("1,200 in")
        );
    }
}
