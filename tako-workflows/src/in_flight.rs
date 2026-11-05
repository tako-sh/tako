//! Per-worker in-flight claim throttling.
//!
//! Tracks how many runs each worker currently holds (claimed but not yet
//! completed/failed/deferred/cancelled). The `claim` path consults this
//! before handing out a run, so a worker can't exceed its concurrency
//! budget even if the SDK fires claim RPCs eagerly.
//!
//! State is in-memory. The durable view lives in the `runs` table (rows
//! with `status='running'` and a valid lease ARE the in-flight set); this
//! struct is just a cached count for O(1) claim-path checks. On process
//! restart, call [`InFlightLimiter::rehydrate`] to reseed from the DB so
//! the cached counts match reality before accepting traffic.
//!
//! Worker process identity is an opaque string. Each claim adds a random suffix. When a
//! worker process dies and a new one starts, its old slots are reclaimed
//! via lease expiry (see `RunsDb::reclaim_expired`), and [`InFlightLimiter::release_claim`] is
//! called for each expired row — so the dead worker's entries drain to
//! zero even though it never called `complete()`.

use parking_lot::Mutex;
use std::collections::HashMap;

/// Keep process accounting separate from ownership of an individual attempt.
/// The random suffix fences old attempts even if the same process reclaims a run.
pub(crate) fn new_claim_owner(worker_id: &str) -> String {
    format!("{worker_id}:{}", nanoid::nanoid!())
}

fn process_worker(claim_owner: &str) -> &str {
    claim_owner
        .rsplit_once(':')
        .map_or(claim_owner, |(worker, _)| worker)
}

pub struct InFlightLimiter {
    max_per_worker: u32,
    counts: Mutex<HashMap<String, u32>>,
}

impl InFlightLimiter {
    /// `max_per_worker` must be ≥ 1. A value of 0 would permanently reject
    /// every claim, so we clamp to at least 1.
    pub fn new(max_per_worker: u32) -> Self {
        Self {
            max_per_worker: max_per_worker.max(1),
            counts: Mutex::new(HashMap::new()),
        }
    }

    pub fn max_per_worker(&self) -> u32 {
        self.max_per_worker
    }

    /// Attempt to reserve one slot for `worker_id`. Returns `true` on
    /// success (caller is now holding a slot and must eventually call
    /// [`release`]); `false` when the worker is already at its cap.
    pub fn try_acquire(&self, worker_id: &str) -> bool {
        let mut counts = self.counts.lock();
        let entry = counts.entry(worker_id.to_string()).or_insert(0);
        if *entry >= self.max_per_worker {
            return false;
        }
        *entry += 1;
        true
    }

    /// Release one slot for `worker_id`. Safe to call even if the count is
    /// already zero (no-op) — defensive, because release paths include
    /// reclaimed leases from dead workers whose accounting may not have
    /// survived a Rust restart.
    pub fn release(&self, worker_id: &str) {
        let mut counts = self.counts.lock();
        if let Some(entry) = counts.get_mut(worker_id)
            && *entry > 0
        {
            *entry -= 1;
            if *entry == 0 {
                counts.remove(worker_id);
            }
        }
    }

    pub fn release_claim(&self, claim_owner: &str) {
        self.release(process_worker(claim_owner));
    }

    /// Replace the current counts with a fresh snapshot. Intended for
    /// rehydration from the DB on startup.
    pub fn rehydrate(&self, snapshot: HashMap<String, u32>) {
        let mut counts = HashMap::<String, u32>::new();
        for (owner, count) in snapshot {
            let total = counts.entry(process_worker(&owner).to_owned()).or_default();
            *total = total.saturating_add(count);
        }
        *self.counts.lock() = counts;
    }

    #[cfg(test)]
    pub fn count(&self, worker_id: &str) -> u32 {
        self.counts.lock().get(worker_id).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_acquire_enforces_cap() {
        let limiter = InFlightLimiter::new(3);
        assert!(limiter.try_acquire("w"));
        assert!(limiter.try_acquire("w"));
        assert!(limiter.try_acquire("w"));
        assert!(!limiter.try_acquire("w"));
        assert_eq!(limiter.count("w"), 3);
    }

    #[test]
    fn release_frees_a_slot() {
        let limiter = InFlightLimiter::new(2);
        assert!(limiter.try_acquire("w"));
        assert!(limiter.try_acquire("w"));
        assert!(!limiter.try_acquire("w"));
        limiter.release("w");
        assert!(limiter.try_acquire("w"));
    }

    #[test]
    fn release_below_zero_is_noop() {
        let limiter = InFlightLimiter::new(1);
        limiter.release("w"); // never acquired
        assert_eq!(limiter.count("w"), 0);
        assert!(limiter.try_acquire("w"));
    }

    #[test]
    fn different_workers_have_independent_counts() {
        let limiter = InFlightLimiter::new(1);
        assert!(limiter.try_acquire("a"));
        assert!(limiter.try_acquire("b"));
        assert!(!limiter.try_acquire("a"));
        assert!(!limiter.try_acquire("b"));
    }

    #[test]
    fn zero_cap_clamps_to_one() {
        let limiter = InFlightLimiter::new(0);
        assert_eq!(limiter.max_per_worker(), 1);
        assert!(limiter.try_acquire("w"));
        assert!(!limiter.try_acquire("w"));
    }

    #[test]
    fn rehydrate_replaces_counts() {
        let limiter = InFlightLimiter::new(5);
        limiter.try_acquire("w");
        let mut snapshot = HashMap::new();
        snapshot.insert("other".to_string(), 4);
        limiter.rehydrate(snapshot);
        assert_eq!(limiter.count("w"), 0);
        assert_eq!(limiter.count("other"), 4);
        assert!(limiter.try_acquire("other"));
        assert!(!limiter.try_acquire("other"));
    }
}

#[cfg(test)]
#[test]
fn claim_owners_share_process_slots_after_rehydration() {
    let limiter = InFlightLimiter::new(2);
    let first = new_claim_owner("host:worker");
    let second = new_claim_owner("host:worker");
    assert_ne!(first, second);
    limiter.rehydrate(HashMap::from([(first.clone(), 1), (second.clone(), 1)]));
    assert_eq!(limiter.count("host:worker"), 2);
    assert!(!limiter.try_acquire("host:worker"));
    limiter.release_claim(&first);
    limiter.release_claim(&second);
    assert_eq!(limiter.count("host:worker"), 0);
    assert!(limiter.try_acquire("host:worker"));
}
