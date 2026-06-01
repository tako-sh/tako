use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

pub(super) const MAX_REQUESTS_PER_IP: u32 = 2048;
pub(crate) const MAX_REQUEST_BODY_BYTES: u64 = 128 * 1024 * 1024;

/// Per-IP concurrent request tracker for basic DDoS mitigation.
pub(super) struct IpRequestTracker {
    connections: dashmap::DashMap<IpAddr, AtomicU32>,
}

impl IpRequestTracker {
    pub(super) fn new() -> Self {
        Self {
            connections: dashmap::DashMap::new(),
        }
    }

    pub(super) fn try_acquire(&self, ip: IpAddr) -> bool {
        let entry = self
            .connections
            .entry(ip)
            .or_insert_with(|| AtomicU32::new(0));
        let prev = entry.value().fetch_add(1, AtomicOrdering::Relaxed);
        if prev >= MAX_REQUESTS_PER_IP {
            entry.value().fetch_sub(1, AtomicOrdering::Relaxed);
            false
        } else {
            true
        }
    }

    pub(super) fn release(&self, ip: IpAddr) {
        if let Some(entry) = self.connections.get(&ip) {
            loop {
                let current = entry.value().load(AtomicOrdering::Relaxed);
                if current == 0 {
                    return;
                }
                if entry
                    .value()
                    .compare_exchange_weak(
                        current,
                        current - 1,
                        AtomicOrdering::Relaxed,
                        AtomicOrdering::Relaxed,
                    )
                    .is_ok()
                {
                    if current == 1 {
                        drop(entry);
                        self.connections
                            .remove_if(&ip, |_, v| v.load(AtomicOrdering::Relaxed) == 0);
                    }
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_allows_requests_until_per_ip_limit() {
        let tracker = IpRequestTracker::new();
        let ip = "203.0.113.10".parse().unwrap();

        for _ in 0..MAX_REQUESTS_PER_IP {
            assert!(tracker.try_acquire(ip));
        }

        assert!(!tracker.try_acquire(ip));
    }

    #[test]
    fn tracker_releases_capacity_for_ip() {
        let tracker = IpRequestTracker::new();
        let ip = "203.0.113.10".parse().unwrap();

        for _ in 0..MAX_REQUESTS_PER_IP {
            assert!(tracker.try_acquire(ip));
        }
        assert!(!tracker.try_acquire(ip));

        tracker.release(ip);

        assert!(tracker.try_acquire(ip));
    }
}
