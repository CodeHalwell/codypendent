//! Failure clustering (STEP 7.4): the improvement input queue.
//!
//! Failed / negative-signal traces are grouped by `(task class, failing signal,
//! tool, error fingerprint)` into [`FailureCluster`]s with exemplar traces
//! ([Chapter 13](../../docs/docs/13-observability-evaluation-learning.md)). A
//! cluster is a recurring, characterized failure mode — the unit a candidate fix
//! (prompt/skill/router/workflow change) targets. Clustering is **deterministic**:
//! the same grades always produce the same clusters in the same order, so a
//! regression that reintroduces a failure lands in the same cluster.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::grade::{Signal, TraceGrade};

/// The key that defines a failure cluster.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClusterKey {
    pub task_class: String,
    pub failing_signal: Signal,
    pub tool: Option<String>,
    pub error_fingerprint: Option<String>,
}

impl ClusterKey {
    /// A stable, **injective** string form (used for deterministic ordering and
    /// lookup): each component is length-prefixed rather than joined with a raw
    /// `|` separator, so no field value — however it embeds `|`, `-`, or any
    /// other separator-look-alike byte — can shift a boundary and collide two
    /// distinct keys into the same string (P7-3). `None` and `Some(s)` are also
    /// kept structurally distinct (an absent field is tagged `N`; a present one
    /// is tagged `S<len>:<value>`), so `tool: None` can never collide with
    /// `tool: Some("-")` the way the previous `unwrap_or("-")` encoding did.
    ///
    /// Matches the length-prefixing style [`crate`]'s sibling crate uses for its
    /// own injective digest (`codypendent_sandbox::verify::signing_digest`,
    /// which length-prefixes its whole canonical payload before hashing) —
    /// applied here per-field since this is a plain lookup key, not a hash.
    #[must_use]
    pub fn as_key(&self) -> String {
        let mut out = String::new();
        push_len_prefixed(&mut out, &self.task_class);
        push_len_prefixed(&mut out, self.failing_signal.as_str());
        push_optional(&mut out, self.tool.as_deref());
        push_optional(&mut out, self.error_fingerprint.as_deref());
        out
    }
}

/// Append `s` as `<byte-length>:<s>` (a netstring-style length prefix). Because
/// the exact byte length is committed *before* the value, nothing inside `s` —
/// including a `:` or any digit — can be misread as marking where the value
/// ends: the decoder (conceptually; this key is never actually decoded back) is
/// always told up front exactly how many bytes to consume.
fn push_len_prefixed(out: &mut String, s: &str) {
    out.push_str(&s.len().to_string());
    out.push(':');
    out.push_str(s);
}

/// Append an `Option<&str>` component, tagging presence so `None` can never
/// collide with a present-but-look-alike value (e.g. `Some("-")`).
fn push_optional(out: &mut String, value: Option<&str>) {
    match value {
        Some(s) => {
            out.push('S');
            push_len_prefixed(out, s);
        }
        None => out.push('N'),
    }
}

/// A group of traces sharing a failure mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureCluster {
    pub key: ClusterKey,
    /// Trace ids in the cluster (sorted, deduplicated).
    pub exemplars: Vec<String>,
}

impl FailureCluster {
    /// How many traces fall in this cluster.
    #[must_use]
    pub fn count(&self) -> usize {
        self.exemplars.len()
    }
}

/// Cluster a batch of trace grades. A trace with several negative signals
/// contributes to one cluster per negative signal (each failure mode is tracked
/// independently). Only traces with a negative signal participate. The output is
/// sorted by cluster key for determinism, and clusters are ordered most-frequent
/// first within that stable ordering is a caller choice — [`rank_by_frequency`].
#[must_use]
pub fn cluster_failures(grades: &[TraceGrade]) -> Vec<FailureCluster> {
    let mut map: BTreeMap<String, (ClusterKey, Vec<String>)> = BTreeMap::new();
    for grade in grades {
        for signal in grade.negative_signals() {
            let key = ClusterKey {
                task_class: grade.task_class.clone(),
                failing_signal: signal,
                tool: grade.tool.clone(),
                error_fingerprint: grade.error_fingerprint.clone(),
            };
            let entry = map
                .entry(key.as_key())
                .or_insert_with(|| (key.clone(), Vec::new()));
            entry.1.push(grade.trace_id.clone());
        }
    }
    map.into_values()
        .map(|(key, mut exemplars)| {
            exemplars.sort();
            exemplars.dedup();
            FailureCluster { key, exemplars }
        })
        .collect()
}

/// Re-order clusters most-frequent first (ties broken by key for determinism) —
/// the priority order for picking what to fix next.
#[must_use]
pub fn rank_by_frequency(mut clusters: Vec<FailureCluster>) -> Vec<FailureCluster> {
    clusters.sort_by(|a, b| {
        b.count()
            .cmp(&a.count())
            .then_with(|| a.key.as_key().cmp(&b.key.as_key()))
    });
    clusters
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grade::{grade, Trace};

    fn failing(id: &str, class: &str, tool: &str, fingerprint: &str, cmd_fail: bool) -> TraceGrade {
        grade(&Trace {
            trace_id: id.into(),
            task_class: class.into(),
            tool: Some(tool.into()),
            error_fingerprint: Some(fingerprint.into()),
            command_failures: u32::from(cmd_fail),
            caused_regression: !cmd_fail,
            ..Default::default()
        })
    }

    #[test]
    fn identical_failures_cluster_together() {
        let grades = vec![
            failing("t1", "small-bug-fix", "cargo", "E0308", true),
            failing("t2", "small-bug-fix", "cargo", "E0308", true),
        ];
        let clusters = cluster_failures(&grades);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count(), 2);
        assert_eq!(clusters[0].exemplars, vec!["t1", "t2"]);
        assert_eq!(clusters[0].key.failing_signal, Signal::CommandFailure);
    }

    #[test]
    fn different_fingerprints_form_different_clusters() {
        let grades = vec![
            failing("t1", "small-bug-fix", "cargo", "E0308", true),
            failing("t2", "small-bug-fix", "cargo", "E0277", true),
        ];
        let clusters = cluster_failures(&grades);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn a_trace_with_two_negatives_lands_in_two_clusters() {
        let g = grade(&Trace {
            trace_id: "t1".into(),
            task_class: "ci-diagnosis".into(),
            tool: Some("cargo".into()),
            command_failures: 1,
            caused_regression: true,
            ..Default::default()
        });
        let clusters = cluster_failures(&[g]);
        // One cluster for command-failure, one for regression.
        assert_eq!(clusters.len(), 2);
        let signals: Vec<Signal> = clusters.iter().map(|c| c.key.failing_signal).collect();
        assert!(signals.contains(&Signal::CommandFailure));
        assert!(signals.contains(&Signal::Regression));
    }

    #[test]
    fn successful_traces_do_not_cluster() {
        let clean = grade(&Trace {
            trace_id: "ok".into(),
            patch_applies: true,
            compiles: true,
            targeted_tests_pass: true,
            ..Default::default()
        });
        assert!(cluster_failures(&[clean]).is_empty());
    }

    #[test]
    fn clustering_is_deterministic() {
        let grades = vec![
            failing("t2", "small-bug-fix", "cargo", "E0308", true),
            failing("t1", "small-bug-fix", "cargo", "E0308", true),
        ];
        let a = cluster_failures(&grades);
        let b = cluster_failures(&grades);
        assert_eq!(a, b);
        // Exemplars are sorted regardless of input order.
        assert_eq!(a[0].exemplars, vec!["t1", "t2"]);
    }

    #[test]
    fn rank_by_frequency_orders_biggest_first() {
        let grades = vec![
            failing("t1", "a", "cargo", "X", true),
            failing("t2", "a", "cargo", "X", true),
            failing("t3", "b", "git", "Y", false),
        ];
        let ranked = rank_by_frequency(cluster_failures(&grades));
        assert_eq!(ranked[0].count(), 2, "the bigger cluster comes first");
    }

    // --- P7-3: the cluster key encoding is injective (no separator collisions) ---

    /// Reproduces the OLD `"{}|{}|{}|{}"` + `unwrap_or("-")` encoding, purely as
    /// evidence that the collisions below are real under the reported bug.
    fn old_buggy_key(k: &ClusterKey) -> String {
        format!(
            "{}|{}|{}|{}",
            k.task_class,
            k.failing_signal.as_str(),
            k.tool.as_deref().unwrap_or("-"),
            k.error_fingerprint.as_deref().unwrap_or("-"),
        )
    }

    #[test]
    fn as_key_does_not_collide_some_dash_with_none() {
        // The review's first named collision: `tool: Some("-")` vs `tool: None`.
        let with_literal_dash = ClusterKey {
            task_class: "task".into(),
            failing_signal: Signal::CommandFailure,
            tool: Some("-".into()),
            error_fingerprint: None,
        };
        let with_none = ClusterKey {
            task_class: "task".into(),
            failing_signal: Signal::CommandFailure,
            tool: None,
            error_fingerprint: None,
        };
        assert_eq!(
            old_buggy_key(&with_literal_dash),
            old_buggy_key(&with_none),
            "sanity: reproduces the reported collision under the old encoding"
        );
        assert_ne!(
            with_literal_dash.as_key(),
            with_none.as_key(),
            "Some(\"-\") must not collide with None under the fixed encoding"
        );
    }

    #[test]
    fn as_key_does_not_collide_on_pipe_injection_across_a_field_boundary() {
        // The review's second named collision: a `|` inside `tool` can shift the
        // separator boundary so a different (tool, error_fingerprint) split
        // renders the same joined string under the old `"{}|{}|{}|{}"` encoding.
        let a = ClusterKey {
            task_class: "task".into(),
            failing_signal: Signal::CommandFailure,
            tool: Some("foo|bar".into()),
            error_fingerprint: None,
        };
        let b = ClusterKey {
            task_class: "task".into(),
            failing_signal: Signal::CommandFailure,
            tool: Some("foo".into()),
            error_fingerprint: Some("bar|-".into()),
        };
        assert_eq!(
            old_buggy_key(&a),
            old_buggy_key(&b),
            "sanity: reproduces the reported `|`-injection collision"
        );
        assert_ne!(
            a.as_key(),
            b.as_key(),
            "a `|` inside a value must not let two different keys collide"
        );
    }

    #[test]
    fn distinct_tool_values_of_none_and_dash_no_longer_merge_into_one_cluster() {
        // The end-to-end pin: two traces that are genuinely different failure
        // modes (no tool reported, vs. a tool literally named "-") must land in
        // two separate clusters, not merge into one via the key collision.
        let grades = vec![
            grade(&Trace {
                trace_id: "t1".into(),
                task_class: "small-bug-fix".into(),
                tool: None,
                command_failures: 1,
                ..Default::default()
            }),
            grade(&Trace {
                trace_id: "t2".into(),
                task_class: "small-bug-fix".into(),
                tool: Some("-".into()),
                command_failures: 1,
                ..Default::default()
            }),
        ];
        let clusters = cluster_failures(&grades);
        assert_eq!(
            clusters.len(),
            2,
            "tool: None and tool: Some(\"-\") are different failure modes and must not merge"
        );
    }

    #[test]
    fn as_key_round_trips_consistently_for_the_same_key() {
        // Determinism: the same key always produces the same string.
        let k = ClusterKey {
            task_class: "small-bug-fix".into(),
            failing_signal: Signal::Regression,
            tool: Some("cargo".into()),
            error_fingerprint: Some("E0308".into()),
        };
        assert_eq!(k.as_key(), k.as_key());
    }
}
