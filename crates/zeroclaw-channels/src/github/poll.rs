//! Polling state machine: per-repo `since` cursors and a bounded
//! seen-ID set. No IO — `channel::listen` drives it against the API.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

/// Evict half the dedup set when it reaches this size (same policy as
/// the Twitter channel).
const DEDUP_CAPACITY: usize = 10_000;

pub struct PollState {
    /// Cold-start floor: events created before this instant are never
    /// processed, so restarts cannot replay history.
    start: DateTime<Utc>,
    /// Per-repo high-water mark of processed `created_at` timestamps.
    cursor: HashMap<String, DateTime<Utc>>,
    /// Recently processed event ids, covering the `since` overlap window.
    seen: HashSet<String>,
}

impl PollState {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            start,
            cursor: HashMap::new(),
            seen: HashSet::new(),
        }
    }

    /// The `since` value to poll this repo with.
    pub fn since(&self, repo_full_name: &str) -> DateTime<Utc> {
        self.cursor
            .get(repo_full_name)
            .copied()
            .unwrap_or(self.start)
    }

    /// Whether an event is fresh: created at-or-after the floor, not yet
    /// seen. Marks it seen.
    pub fn admit(&mut self, id: &str, created_at: DateTime<Utc>) -> bool {
        if created_at < self.start {
            return false;
        }
        if self.seen.contains(id) {
            return false;
        }
        if self.seen.len() >= DEDUP_CAPACITY {
            let evict: Vec<String> = self.seen.iter().take(DEDUP_CAPACITY / 2).cloned().collect();
            for key in evict {
                self.seen.remove(&key);
            }
        }
        self.seen.insert(id.to_string());
        true
    }

    /// Advance the repo cursor to cover a processed event.
    pub fn advance(&mut self, repo_full_name: &str, created_at: DateTime<Utc>) {
        let entry = self
            .cursor
            .entry(repo_full_name.to_string())
            .or_insert(self.start);
        if created_at > *entry {
            *entry = created_at;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn state() -> (PollState, DateTime<Utc>) {
        let start = Utc::now();
        (PollState::new(start), start)
    }

    #[test]
    fn since_falls_back_to_start_then_tracks_advance() {
        let (mut s, start) = state();
        assert_eq!(s.since("o/r"), start);
        let later = start + Duration::seconds(90);
        s.advance("o/r", later);
        assert_eq!(s.since("o/r"), later);
        // Advancing backwards is a no-op.
        s.advance("o/r", start);
        assert_eq!(s.since("o/r"), later);
    }

    #[test]
    fn admit_rejects_pre_start_events() {
        let (mut s, start) = state();
        assert!(!s.admit("ghc_1", start - Duration::seconds(5)));
        assert!(s.admit("ghc_2", start + Duration::seconds(5)));
    }

    #[test]
    fn admit_dedups_repeated_ids() {
        let (mut s, start) = state();
        let t = start + Duration::seconds(1);
        assert!(s.admit("ghc_1", t));
        assert!(!s.admit("ghc_1", t));
    }

    #[test]
    fn cursors_are_per_repo() {
        let (mut s, start) = state();
        s.advance("a/a", start + Duration::seconds(100));
        assert_eq!(s.since("b/b"), start);
    }
}
