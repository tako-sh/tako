use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

// Keep production conservative. Benchmarks that intentionally drive one source
// IP past this ceiling should set TAKO_MAX_REQUESTS_PER_IP instead.
pub(super) const DEFAULT_MAX_REQUESTS_PER_IP: u32 = 2048;
pub(super) const MAX_REQUESTS_PER_IP_ENV: &str = "TAKO_MAX_REQUESTS_PER_IP";
pub(crate) const MAX_REQUEST_BODY_BYTES: u64 = 128 * 1024 * 1024;

/// Per-IP concurrent request tracker for basic DDoS mitigation.
pub(super) struct IpRequestTracker {
    connections: dashmap::DashMap<IpAddr, AtomicU32>,
    max_requests_per_ip: u32,
}

impl IpRequestTracker {
    pub(super) fn new() -> Self {
        Self::with_limit(max_requests_per_ip_from_env())
    }

    pub(super) fn with_limit(max_requests_per_ip: u32) -> Self {
        Self {
            connections: dashmap::DashMap::new(),
            max_requests_per_ip,
        }
    }

    pub(super) fn try_acquire(&self, ip: IpAddr) -> bool {
        if let Some(entry) = self.connections.get(&ip) {
            return self.try_increment(entry.value());
        }

        let entry = self
            .connections
            .entry(ip)
            .or_insert_with(|| AtomicU32::new(0));
        self.try_increment(entry.value())
    }

    fn try_increment(&self, counter: &AtomicU32) -> bool {
        let prev = counter.fetch_add(1, AtomicOrdering::Relaxed);
        if prev >= self.max_requests_per_ip {
            counter.fetch_sub(1, AtomicOrdering::Relaxed);
            false
        } else {
            true
        }
    }

    pub(super) fn release(&self, ip: IpAddr) {
        if let Some(entry) = self.connections.get(&ip) {
            let current = entry.value().fetch_sub(1, AtomicOrdering::Relaxed);
            if current == 1 {
                drop(entry);
                self.connections
                    .remove_if(&ip, |_, v| v.load(AtomicOrdering::Relaxed) == 0);
            }
        }
    }
}

fn max_requests_per_ip_from_env() -> u32 {
    let Some(raw) = std::env::var_os(MAX_REQUESTS_PER_IP_ENV) else {
        return DEFAULT_MAX_REQUESTS_PER_IP;
    };
    let raw = raw.to_string_lossy();
    match parse_request_limit(&raw) {
        Some(limit) => limit,
        None => {
            tracing::warn!(
                env = MAX_REQUESTS_PER_IP_ENV,
                value = %raw,
                default = DEFAULT_MAX_REQUESTS_PER_IP,
                "Ignoring invalid per-IP request limit override"
            );
            DEFAULT_MAX_REQUESTS_PER_IP
        }
    }
}

fn parse_request_limit(value: &str) -> Option<u32> {
    let limit = value.trim().parse::<u32>().ok()?;
    (limit > 0).then_some(limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_limit_override() {
        assert_eq!(parse_request_limit("65536"), Some(65_536));
        assert_eq!(parse_request_limit(" 4096 "), Some(4096));
    }

    #[test]
    fn rejects_invalid_request_limit_override() {
        assert_eq!(parse_request_limit("0"), None);
        assert_eq!(parse_request_limit(""), None);
        assert_eq!(parse_request_limit("many"), None);
    }

    #[test]
    fn tracker_allows_requests_until_per_ip_limit() {
        let tracker = IpRequestTracker::with_limit(3);
        let ip = "203.0.113.10".parse().unwrap();

        for _ in 0..3 {
            assert!(tracker.try_acquire(ip));
        }

        assert!(!tracker.try_acquire(ip));
    }

    #[test]
    fn tracker_releases_capacity_for_ip() {
        let tracker = IpRequestTracker::with_limit(3);
        let ip = "203.0.113.10".parse().unwrap();

        for _ in 0..3 {
            assert!(tracker.try_acquire(ip));
        }
        assert!(!tracker.try_acquire(ip));

        tracker.release(ip);

        assert!(tracker.try_acquire(ip));
    }
}
