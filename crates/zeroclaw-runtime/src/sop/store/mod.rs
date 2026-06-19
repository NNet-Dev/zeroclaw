//! Durable SOP run-state store (EPIC B) — the keystone contract.
//!
//! A single [`SopRunStore`] is owned by the engine singleton (EPIC A). It is the
//! one durable home for run state, the CAS-claim admission primitive
//! (concurrency-control), the append-only event log (audit-trail / observability),
//! and the procedural-memory proposal namespace — so those epics ride **one**
//! abstraction, not three.
//!
//! This module currently ships the trait + wire shapes + an in-memory default
//! impl (mirrors today's in-memory behaviour, zero behaviour change). The
//! durable `SqliteRunStore`, the `Memory`-backed adapter, boot-rehydrate, and the
//! engine wiring are follow-on commits — see
//! `epics/B-run-state-store/{03-architecture,04-implementation-plan}.md`.

pub mod model;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub use model::{
    ClaimToken, PersistedRun, ProposalRecord, ProposalStatus, RetentionPolicy, SOP_STORE_VERSION,
    SopEventRecord,
};

/// First-class durable run-state store. ONE per engine singleton.
///
/// All methods are sync and **fail-loud**: a store error is never silently
/// swallowed (persistence is fail-closed). Implementations must be cheap to
/// `Arc::clone` and safe to share across the daemon tick, agent tools, MQTT
/// listener, and the gateway approve surface.
pub trait SopRunStore: Send + Sync {
    // ── run state (persistence-resume, state-machine) ──
    /// Persist-before-mutate. Monotonic-revision-guarded; idempotent on revision.
    fn save_run(&self, run: &PersistedRun) -> Result<(), StoreError>;
    /// Move a run to terminal state (kept as a terminal record, not deleted) and
    /// release any live claim.
    fn finish_run(&self, run_id: &str, terminal: &PersistedRun) -> Result<(), StoreError>;
    /// Boot-rehydrate source: every non-terminal run (latest revision per id).
    fn load_active_runs(&self) -> Result<Vec<PersistedRun>, StoreError>;
    /// Single run by id (latest revision), terminal or not.
    fn load_run(&self, run_id: &str) -> Result<Option<PersistedRun>, StoreError>;

    // ── CAS claim primitive (concurrency-control) ──
    /// Atomic single-winner admission. Returns `Some(token)` to exactly one
    /// caller iff under `cap` and no live claim exists for `run_id`, else `None`.
    fn try_claim_run(
        &self,
        run_id: &str,
        sop_name: &str,
        cap: usize,
    ) -> Result<Option<ClaimToken>, StoreError>;
    /// Renew a claim's lease (tick liveness). No-op if the claim is gone.
    fn heartbeat_claim(&self, token: &ClaimToken) -> Result<(), StoreError>;
    /// Release a claim (finish/cancel), freeing the slot for admission.
    fn release_claim(&self, token: &ClaimToken) -> Result<(), StoreError>;
    /// Reaper source: claims whose lease expired at/<= `now_iso`.
    fn expired_claims(&self, now_iso: &str) -> Result<Vec<ClaimToken>, StoreError>;

    // ── append-only event log (audit-trail, observability) ──
    /// Append-only, monotonic-seq, never-overwrite. Returns the assigned seq.
    fn append_event(&self, ev: &SopEventRecord) -> Result<u64, StoreError>;
    /// Ordered event history for a run.
    fn list_events(&self, run_id: &str) -> Result<Vec<SopEventRecord>, StoreError>;

    // ── proposal namespace (procedural-memory — strictly last consumer) ──
    fn save_proposal(&self, p: &ProposalRecord) -> Result<(), StoreError>;
    fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError>;
    fn list_proposals(
        &self,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<ProposalRecord>, StoreError>;

    // ── maintenance ──
    /// Drop terminal runs beyond the retention policy. Returns the count dropped.
    fn prune(&self, policy: &RetentionPolicy) -> Result<usize, StoreError>;
    fn health_check(&self) -> bool;
    /// Backend name (for logs + the "never a silent no-op" guard).
    fn backend(&self) -> &'static str;
}

/// Errors a store may surface. Never swallowed by callers.
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    Backend(String),
    /// A save lost the revision race (a newer revision already persisted).
    StaleRevision {
        run_id: String,
        have: u64,
        found: u64,
    },
    /// A claim was lost to a concurrent winner (over-cap or already claimed).
    ClaimLost,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "sop store io error: {e}"),
            Self::Serde(e) => write!(f, "sop store serde error: {e}"),
            Self::Backend(m) => write!(f, "sop store backend error: {m}"),
            Self::StaleRevision {
                run_id,
                have,
                found,
            } => write!(
                f,
                "sop store stale revision for run {run_id}: have {have}, found {found}"
            ),
            Self::ClaimLost => write!(f, "sop store claim lost to a concurrent winner"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

/// Build the configured run store.
///
/// Default backend is in-memory (current behaviour). The config-driven
/// SQLite/Memory selection lands with `SqliteRunStore` (follow-on commit); this
/// keeps the contract stable for callers in the meantime.
pub fn build_run_store() -> Arc<dyn SopRunStore> {
    Arc::new(InMemoryRunStore::new())
}

// ── In-memory default backend ──────────────────────────────────────

#[derive(Default)]
struct Inner {
    runs: HashMap<String, PersistedRun>,
    terminal: HashSet<String>,
    events: HashMap<String, Vec<SopEventRecord>>,
    claims: HashMap<String, ClaimToken>,
    proposals: HashMap<String, ProposalRecord>,
    seq: u64,
}

/// Process-local, non-durable store. Mirrors today's in-memory run maps; lost on
/// restart. The compatibility default until `SqliteRunStore` lands.
pub struct InMemoryRunStore {
    inner: Mutex<Inner>,
}

impl Default for InMemoryRunStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryRunStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, StoreError> {
        self.inner
            .lock()
            .map_err(|_| StoreError::Backend("in-memory store lock poisoned".into()))
    }
}

impl SopRunStore for InMemoryRunStore {
    fn save_run(&self, run: &PersistedRun) -> Result<(), StoreError> {
        let mut g = self.lock()?;
        let id = run.run_id().to_string();
        if let Some(existing) = g.runs.get(&id)
            && existing.revision > run.revision
        {
            return Err(StoreError::StaleRevision {
                run_id: id,
                have: run.revision,
                found: existing.revision,
            });
        }
        g.runs.insert(id, run.clone());
        Ok(())
    }

    fn finish_run(&self, run_id: &str, terminal: &PersistedRun) -> Result<(), StoreError> {
        let mut g = self.lock()?;
        g.runs.insert(run_id.to_string(), terminal.clone());
        g.terminal.insert(run_id.to_string());
        g.claims.remove(run_id);
        Ok(())
    }

    fn load_active_runs(&self) -> Result<Vec<PersistedRun>, StoreError> {
        let g = self.lock()?;
        Ok(g.runs
            .values()
            .filter(|r| !g.terminal.contains(r.run_id()))
            .cloned()
            .collect())
    }

    fn load_run(&self, run_id: &str) -> Result<Option<PersistedRun>, StoreError> {
        Ok(self.lock()?.runs.get(run_id).cloned())
    }

    fn try_claim_run(
        &self,
        run_id: &str,
        sop_name: &str,
        cap: usize,
    ) -> Result<Option<ClaimToken>, StoreError> {
        let mut g = self.lock()?;
        if g.claims.contains_key(run_id) {
            return Ok(None);
        }
        // A run that has already reached a terminal state is not re-claimable.
        if g.terminal.contains(run_id) {
            return Ok(None);
        }
        if cap != 0 && g.claims.len() >= cap {
            return Ok(None);
        }
        // Timestamps are stamped by durable backends; the in-memory backend
        // leaves them empty (the reaper skips empty leases — see `expired_claims`).
        let token = ClaimToken {
            run_id: run_id.to_string(),
            sop_name: sop_name.to_string(),
            claimed_at: String::new(),
            lease_expires: String::new(),
            holder: "in-memory".to_string(),
        };
        g.claims.insert(run_id.to_string(), token.clone());
        Ok(Some(token))
    }

    fn heartbeat_claim(&self, _token: &ClaimToken) -> Result<(), StoreError> {
        Ok(())
    }

    fn release_claim(&self, token: &ClaimToken) -> Result<(), StoreError> {
        self.lock()?.claims.remove(&token.run_id);
        Ok(())
    }

    fn expired_claims(&self, now_iso: &str) -> Result<Vec<ClaimToken>, StoreError> {
        let g = self.lock()?;
        Ok(g.claims
            .values()
            .filter(|c| !c.lease_expires.is_empty() && c.lease_expires.as_str() <= now_iso)
            .cloned()
            .collect())
    }

    fn append_event(&self, ev: &SopEventRecord) -> Result<u64, StoreError> {
        let mut g = self.lock()?;
        g.seq += 1;
        let seq = g.seq;
        let mut rec = ev.clone();
        rec.seq = seq;
        g.events.entry(ev.run_id.clone()).or_default().push(rec);
        Ok(seq)
    }

    fn list_events(&self, run_id: &str) -> Result<Vec<SopEventRecord>, StoreError> {
        let mut v = self.lock()?.events.get(run_id).cloned().unwrap_or_default();
        v.sort_by_key(|e| e.seq);
        Ok(v)
    }

    fn save_proposal(&self, p: &ProposalRecord) -> Result<(), StoreError> {
        self.lock()?.proposals.insert(p.id.clone(), p.clone());
        Ok(())
    }

    fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError> {
        Ok(self.lock()?.proposals.get(id).cloned())
    }

    fn list_proposals(
        &self,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<ProposalRecord>, StoreError> {
        let g = self.lock()?;
        Ok(g.proposals
            .values()
            .filter(|p| status.is_none_or(|s| p.status == s))
            .cloned()
            .collect())
    }

    fn prune(&self, policy: &RetentionPolicy) -> Result<usize, StoreError> {
        if policy.max_terminal == 0 {
            return Ok(0);
        }
        let mut g = self.lock()?;
        if g.terminal.len() <= policy.max_terminal {
            return Ok(0);
        }
        // Evict OLDEST terminal runs first (by completion, then start time) —
        // HashSet order is non-deterministic, so sort before truncating.
        let mut terminal: Vec<(String, String)> = g
            .terminal
            .iter()
            .map(|id| {
                let ts = g
                    .runs
                    .get(id)
                    .map(|r| {
                        r.run
                            .completed_at
                            .clone()
                            .unwrap_or_else(|| r.run.started_at.clone())
                    })
                    .unwrap_or_default();
                (id.clone(), ts)
            })
            .collect();
        terminal.sort_by(|a, b| a.1.cmp(&b.1));
        let drop_n = terminal.len() - policy.max_terminal;
        for (id, _) in terminal.into_iter().take(drop_n) {
            g.runs.remove(&id);
            g.events.remove(&id);
            g.terminal.remove(&id);
        }
        Ok(drop_n)
    }

    fn health_check(&self) -> bool {
        self.inner.lock().is_ok()
    }

    fn backend(&self) -> &'static str {
        "in-memory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(run: &str, kind: &str) -> SopEventRecord {
        SopEventRecord {
            run_id: run.to_string(),
            seq: 0,
            ts: "t".to_string(),
            kind: kind.to_string(),
            actor: None,
            reason: None,
            payload: json!({}),
        }
    }

    fn proposal(id: &str, status: ProposalStatus) -> ProposalRecord {
        ProposalRecord {
            id: id.to_string(),
            status,
            source_run_id: None,
            sop_name: "deploy".to_string(),
            target_content_hash: None,
            provenance: json!({}),
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
        }
    }

    #[test]
    fn claim_is_single_winner_and_cap_bounded() {
        let s = build_run_store();
        // First claim wins.
        assert!(s.try_claim_run("r1", "deploy", 2).unwrap().is_some());
        // Duplicate claim on the same run is refused.
        assert!(s.try_claim_run("r1", "deploy", 2).unwrap().is_none());
        // Second distinct run claims (under cap).
        assert!(s.try_claim_run("r2", "deploy", 2).unwrap().is_some());
        // Third exceeds cap=2.
        assert!(s.try_claim_run("r3", "deploy", 2).unwrap().is_none());
        // Releasing frees a slot.
        let tok = ClaimToken {
            run_id: "r1".to_string(),
            sop_name: "deploy".to_string(),
            claimed_at: String::new(),
            lease_expires: String::new(),
            holder: "in-memory".to_string(),
        };
        s.release_claim(&tok).unwrap();
        assert!(s.try_claim_run("r3", "deploy", 2).unwrap().is_some());
    }

    #[test]
    fn events_are_append_only_with_monotonic_seq() {
        let s = build_run_store();
        assert_eq!(s.append_event(&ev("r1", "run_started")).unwrap(), 1);
        assert_eq!(s.append_event(&ev("r1", "step_completed")).unwrap(), 2);
        assert_eq!(s.append_event(&ev("r2", "run_started")).unwrap(), 3);
        let r1 = s.list_events("r1").unwrap();
        assert_eq!(r1.len(), 2);
        assert_eq!(r1[0].seq, 1);
        assert_eq!(r1[1].kind, "step_completed");
        assert_eq!(s.list_events("r2").unwrap().len(), 1);
    }

    #[test]
    fn proposals_round_trip_and_filter_by_status() {
        let s = build_run_store();
        s.save_proposal(&proposal("p1", ProposalStatus::Pending))
            .unwrap();
        s.save_proposal(&proposal("p2", ProposalStatus::Applied))
            .unwrap();
        assert_eq!(
            s.load_proposal("p1").unwrap().unwrap().status,
            ProposalStatus::Pending
        );
        assert_eq!(s.list_proposals(None).unwrap().len(), 2);
        assert_eq!(
            s.list_proposals(Some(ProposalStatus::Applied))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(s.backend(), "in-memory");
    }
}
