//! Single-use, TTL-swept registry binding a component `custom_id` to the
//! server-side intent it resolves.
//!
//! A component click echoes back only the `custom_id` we put on it — a
//! client-controlled string. So the *meaning* of a component must never be
//! trusted from the wire: the sender registers the intent here when it emits the
//! component, and the inbound dispatch `take`s it (removing it) on the click. A
//! `custom_id` that isn't in the registry — forged, replayed, or expired —
//! resolves to nothing and is refused, so a crafted id can't drive an arbitrary
//! action. This is the replay/forgery half of the component security model; the
//! per-click `interaction_gate` (fail-closed authz) is the other half.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Components live as long as their interaction-followup token (15 min); a click
/// after that can't be answered anyway.
const COMPONENT_TTL: Duration = Duration::from_secs(15 * 60);

/// What a registered component does when clicked. Phase 2 resolves into an agent
/// turn; EPIC B Phase 4 adds an approval variant that resolves a parked
/// `oneshot` instead of enqueuing.
// Constructed by the *sender* side (a feature that emits a component +
// registers its intent) — the first such caller lands in Phase 4 (buttoned
// approval). The dispatch already `take`s and matches it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ComponentIntent {
    /// Enqueue this prompt as an agent turn — the click drives the agent, whose
    /// reply is delivered through the interaction followup.
    ResolveIntoTurn { prompt: String },
}

struct Entry {
    intent: ComponentIntent,
    created: Instant,
}

/// Channel-local registry of live components, keyed by the full `custom_id`
/// (already a `zc1` token). Single-use: `take` removes the entry.
#[derive(Default)]
pub(crate) struct PendingComponents {
    entries: HashMap<String, Entry>,
}

impl PendingComponents {
    /// Register a component's intent under its `custom_id`, sweeping expired
    /// entries first (bounded by the emit rate, so the map can't grow without
    /// bound from never-clicked components).
    // Sender-side: first caller is Phase 4 (emit a component, register its
    // intent). `take` (the dispatch consumer) is already live.
    #[allow(dead_code)]
    pub(crate) fn register(&mut self, custom_id: String, intent: ComponentIntent) {
        self.sweep();
        self.entries.insert(
            custom_id,
            Entry {
                intent,
                created: Instant::now(),
            },
        );
    }

    /// Remove and return the intent for `custom_id` iff it is live (present and
    /// un-expired). Single-use: a second `take` of the same id returns `None`
    /// (replay refused); an absent or expired id returns `None`
    /// (forged/stale → the caller fails closed).
    pub(crate) fn take(&mut self, custom_id: &str) -> Option<ComponentIntent> {
        let entry = self.entries.remove(custom_id)?;
        (entry.created.elapsed() < COMPONENT_TTL).then_some(entry.intent)
    }

    // Only reached via `register` (sender-side), so dead until Phase 4.
    #[allow(dead_code)]
    fn sweep(&mut self) {
        self.entries
            .retain(|_, e| e.created.elapsed() < COMPONENT_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(p: &str) -> ComponentIntent {
        ComponentIntent::ResolveIntoTurn { prompt: p.into() }
    }

    #[test]
    fn registered_component_resolves_once() {
        let mut reg = PendingComponents::default();
        reg.register("zc1|approve|i1".into(), turn("approve it"));
        assert_eq!(reg.take("zc1|approve|i1"), Some(turn("approve it")));
    }

    #[test]
    fn second_click_is_refused_replay() {
        let mut reg = PendingComponents::default();
        reg.register("zc1|approve|i1".into(), turn("approve it"));
        assert!(reg.take("zc1|approve|i1").is_some());
        assert_eq!(
            reg.take("zc1|approve|i1"),
            None,
            "single-use: replay refused"
        );
    }

    #[test]
    fn forged_or_unknown_custom_id_resolves_to_none() {
        let mut reg = PendingComponents::default();
        reg.register("zc1|approve|i1".into(), turn("approve it"));
        assert_eq!(reg.take("zc1|approve|forged"), None);
        assert_eq!(reg.take("anything-else"), None);
    }

    #[test]
    fn expired_entry_is_swept_and_refused() {
        let mut reg = PendingComponents::default();
        // Insert a manually-aged entry, then confirm take() refuses it.
        reg.entries.insert(
            "zc1|old|i1".into(),
            Entry {
                intent: turn("stale"),
                created: Instant::now() - (COMPONENT_TTL + Duration::from_secs(1)),
            },
        );
        assert_eq!(reg.take("zc1|old|i1"), None, "expired entry refused");
    }

    #[test]
    fn register_sweeps_expired_entries() {
        let mut reg = PendingComponents::default();
        reg.entries.insert(
            "zc1|old|i1".into(),
            Entry {
                intent: turn("stale"),
                created: Instant::now() - (COMPONENT_TTL + Duration::from_secs(1)),
            },
        );
        reg.register("zc1|fresh|i2".into(), turn("fresh"));
        assert!(
            !reg.entries.contains_key("zc1|old|i1"),
            "expired entry swept on register"
        );
        assert!(reg.entries.contains_key("zc1|fresh|i2"));
    }
}
