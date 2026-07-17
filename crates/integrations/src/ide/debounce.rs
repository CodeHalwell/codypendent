//! Coalescing bursts of IDE context updates (Chapter 10, "debounced updates").
//!
//! A stream of keystrokes produces a stream of [`IdeContextUpdate`]s; the daemon
//! only needs the *settled* state, not every intermediate. Both tools here are
//! deterministic and time-injected — logical millisecond timestamps are passed
//! in rather than read from a clock — so behavior is reproducible in tests.
//!
//! [`coalesce_bursts`] is the batch form over a recorded event log;
//! [`Debouncer`] is the streaming form the daemon drives with `observe`/`poll`.

use codypendent_protocol::ide::IdeContextUpdate;

/// Collapse ascending `(timestamp_ms, update)` pairs to one update per burst.
///
/// An update is emitted when the gap to the *next* event is at least
/// `window_ms`; the final event always flushes. Two events closer than
/// `window_ms` belong to the same burst, and only the last update of a burst is
/// kept. Input is assumed to be in ascending time order.
pub fn coalesce_bursts(
    events: &[(u64, IdeContextUpdate)],
    window_ms: u64,
) -> Vec<IdeContextUpdate> {
    let mut out = Vec::new();
    for (index, (timestamp_ms, update)) in events.iter().enumerate() {
        let flush = match events.get(index + 1) {
            Some((next_ms, _)) => next_ms.saturating_sub(*timestamp_ms) >= window_ms,
            None => true,
        };
        if flush {
            out.push(update.clone());
        }
    }
    out
}

/// A streaming coalescer: `observe` records the latest update as pending, and
/// `poll` emits that pending update once the window has elapsed with no newer
/// observation. Time is supplied by the caller as logical milliseconds.
#[derive(Debug, Clone, Default)]
pub struct Debouncer {
    window_ms: u64,
    pending: Option<IdeContextUpdate>,
    last_seen_ms: Option<u64>,
}

impl Debouncer {
    /// Construct a debouncer with the given quiet-window in milliseconds.
    pub fn new(window_ms: u64) -> Self {
        Self {
            window_ms,
            pending: None,
            last_seen_ms: None,
        }
    }

    /// Record `update` as the latest pending state, observed at `now_ms`.
    pub fn observe(&mut self, now_ms: u64, update: IdeContextUpdate) {
        self.pending = Some(update);
        self.last_seen_ms = Some(now_ms);
    }

    /// Emit the pending update if at least `window_ms` has elapsed since the
    /// last observation; otherwise `None`. Once emitted, the pending state is
    /// cleared so a subsequent poll before the next `observe` yields `None`.
    pub fn poll(&mut self, now_ms: u64) -> Option<IdeContextUpdate> {
        let last_seen = self.last_seen_ms?;
        if now_ms.saturating_sub(last_seen) >= self.window_ms {
            self.last_seen_ms = None;
            self.pending.take()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a distinguishable update: the diagnostics revision tags which one.
    fn update(tag: u64) -> IdeContextUpdate {
        IdeContextUpdate {
            diagnostics_revision: tag,
            ..IdeContextUpdate::default()
        }
    }

    #[test]
    fn burst_within_window_collapses_to_one() {
        let events = vec![
            (0, update(0)),
            (50, update(1)),
            (100, update(2)),
            (150, update(3)),
            (200, update(4)),
        ];
        let out = coalesce_bursts(&events, 300);
        assert_eq!(out.len(), 1);
        // The LAST update of the burst is the one kept.
        assert_eq!(out[0].diagnostics_revision, 4);
    }

    #[test]
    fn two_bursts_separated_by_a_gap_collapse_to_two() {
        let events = vec![
            (0, update(0)),
            (50, update(1)),
            (100, update(2)),
            (150, update(3)),
            (200, update(4)),
            (600, update(5)),
        ];
        let out = coalesce_bursts(&events, 300);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].diagnostics_revision, 4);
        assert_eq!(out[1].diagnostics_revision, 5);
    }

    #[test]
    fn debouncer_emits_last_pending_after_window() {
        let mut debouncer = Debouncer::new(300);
        debouncer.observe(0, update(0));
        debouncer.observe(50, update(1));
        debouncer.observe(100, update(2));
        // Still within the window: nothing settled yet.
        assert!(debouncer.poll(200).is_none());
        // Window elapsed since the last observation at t=100.
        let emitted = debouncer.poll(400).expect("emit");
        assert_eq!(emitted.diagnostics_revision, 2);
        // Nothing left pending.
        assert!(debouncer.poll(1000).is_none());
    }
}
