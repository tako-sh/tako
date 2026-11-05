//! Per-worker in-flight claim throttling.
//!
//! Cache individual claim owners so delayed releases are idempotent. At the
//! worker's concurrency cap, refresh from the database: another server may have
//! reclaimed a lease without being able to update this server's cache.

use parking_lot::Mutex;
use std::collections::HashMap;
use tako_core::RunPayload;

use crate::enqueue::{RunsDb, RunsDbError};

/// Keep process accounting separate from ownership of an individual attempt.
fn new_claim_owner(worker_id: &str) -> String {
    format!("{worker_id}:{}", nanoid::nanoid!())
}

fn process_worker(claim_owner: &str) -> &str {
    claim_owner
        .rsplit_once(':')
        .map_or(claim_owner, |(worker, _)| worker)
}

fn count_worker(claims: &HashMap<String, u32>, worker_id: &str) -> u32 {
    claims
        .iter()
        .filter(|(owner, _)| process_worker(owner) == worker_id)
        .fold(0u32, |total, (_, count)| total.saturating_add(*count))
}

pub struct InFlightLimiter {
    max_per_worker: u32,
    claims: Mutex<HashMap<String, u32>>,
}

impl InFlightLimiter {
    /// Clamp a zero budget to one so workers can always make progress.
    pub fn new(max_per_worker: u32) -> Self {
        Self {
            max_per_worker: max_per_worker.max(1),
            claims: Mutex::new(HashMap::new()),
        }
    }

    pub fn max_per_worker(&self) -> u32 {
        self.max_per_worker
    }

    pub fn claim(
        &self,
        db: &RunsDb,
        worker_id: &str,
        names: &[String],
        lease_ms: u64,
    ) -> Result<Option<RunPayload>, RunsDbError> {
        // Serialize admission, database refresh, and claim commit. Otherwise a
        // refresh could erase a reservation whose database claim is in flight.
        // Store operations release their connection lock before releasing a
        // claim here, so the lock order is always limiter -> connection.
        let mut claims = self.claims.lock();
        if count_worker(&claims, worker_id) >= self.max_per_worker {
            *claims = db.in_flight_by_worker()?;
            if count_worker(&claims, worker_id) >= self.max_per_worker {
                return Ok(None);
            }
        }
        let owner = new_claim_owner(worker_id);
        let run = db.claim(&owner, names, lease_ms)?;
        if run.is_some() {
            claims.insert(owner, 1);
        }
        Ok(run)
    }

    /// Release exactly this claim. A refresh may already have removed it, and
    /// its delayed completion/reclaim must not free a newer claim's slot.
    pub fn release_claim(&self, claim_owner: &str) {
        let mut claims = self.claims.lock();
        if let Some(count) = claims.get_mut(claim_owner) {
            if *count > 1 {
                *count -= 1;
            } else {
                claims.remove(claim_owner);
            }
        }
    }

    /// Seed from the database before accepting claims at startup.
    pub fn rehydrate(&self, snapshot: HashMap<String, u32>) {
        *self.claims.lock() = snapshot;
    }

    #[cfg(test)]
    pub fn count(&self, worker_id: &str) -> u32 {
        count_worker(&self.claims.lock(), worker_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use tako_core::EnqueueOpts;

    fn queue() -> RunsDb {
        let db = RunsDb::open_in_memory().unwrap();
        for _ in 0..6 {
            db.enqueue("work", &serde_json::json!({}), &EnqueueOpts::default())
                .unwrap();
        }
        db
    }

    fn claim(limiter: &InFlightLimiter, db: &RunsDb, worker: &str) -> Option<RunPayload> {
        limiter.claim(db, worker, &["work".into()], 60_000).unwrap()
    }

    #[test]
    fn claims_enforce_cap_and_release_exact_owner() {
        let db = queue();
        let limiter = InFlightLimiter::new(2);
        let first = claim(&limiter, &db, "worker").unwrap();
        claim(&limiter, &db, "worker").unwrap();
        assert!(claim(&limiter, &db, "worker").is_none());
        assert_eq!(db.pending_count().unwrap(), 4);
        db.complete(&first.id, &first.worker_id).unwrap();
        limiter.release_claim(&first.worker_id);
        claim(&limiter, &db, "worker").unwrap();
        limiter.release_claim(&first.worker_id);
        assert_eq!(limiter.count("worker"), 2);
        assert!(claim(&limiter, &db, "worker").is_none());
    }

    #[test]
    fn different_workers_have_independent_budgets() {
        let db = queue();
        let limiter = InFlightLimiter::new(1);
        claim(&limiter, &db, "a").unwrap();
        claim(&limiter, &db, "b").unwrap();
        assert!(claim(&limiter, &db, "a").is_none());
        assert!(claim(&limiter, &db, "b").is_none());
    }

    #[test]
    fn zero_cap_clamps_to_one() {
        let db = queue();
        let limiter = InFlightLimiter::new(0);
        assert_eq!(limiter.max_per_worker(), 1);
        claim(&limiter, &db, "worker").unwrap();
        assert!(claim(&limiter, &db, "worker").is_none());
    }

    #[test]
    fn claim_owners_share_process_slots_after_rehydration() {
        let db = queue();
        let original = InFlightLimiter::new(2);
        let first = claim(&original, &db, "host:worker").unwrap();
        let second = claim(&original, &db, "host:worker").unwrap();
        assert_ne!(first.worker_id, second.worker_id);
        let restarted = InFlightLimiter::new(2);
        restarted.rehydrate(db.in_flight_by_worker().unwrap());
        assert_eq!(restarted.count("host:worker"), 2);
        assert!(claim(&restarted, &db, "host:worker").is_none());
        db.complete(&first.id, &first.worker_id).unwrap();
        db.complete(&second.id, &second.worker_id).unwrap();
        restarted.release_claim(&first.worker_id);
        restarted.release_claim(&second.worker_id);
        assert_eq!(restarted.count("host:worker"), 0);
        claim(&restarted, &db, "host:worker").unwrap();
    }

    #[test]
    fn concurrent_claims_cannot_overbook_during_refresh() {
        let db = Arc::new(queue());
        let limiter = Arc::new(InFlightLimiter::new(2));
        let barrier = Arc::new(Barrier::new(6));
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let (db, limiter, barrier) = (db.clone(), limiter.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    claim(&limiter, &db, "worker")
                })
            })
            .collect();
        let claimed = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .count();
        assert_eq!(claimed, 2);
        assert_eq!(limiter.count("worker"), 2);
        assert_eq!(db.pending_count().unwrap(), 4);
    }

    #[test]
    fn failed_claim_does_not_reserve_a_slot() {
        let db = queue();
        let limiter = InFlightLimiter::new(1);
        db.raw_execute("CREATE TRIGGER reject_claim BEFORE UPDATE ON runs BEGIN SELECT RAISE(ABORT, 'injected claim failure'); END", ());
        assert!(
            limiter
                .claim(&db, "worker", &["work".into()], 60_000)
                .is_err()
        );
        assert_eq!(limiter.count("worker"), 0);
        db.raw_execute("DROP TRIGGER reject_claim", ());
        claim(&limiter, &db, "worker").unwrap();
    }
}
