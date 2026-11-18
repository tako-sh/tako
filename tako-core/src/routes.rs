//! Route pattern matching shared by the CLI and servers.
//!
//! A route is a hostname, exact (`api.example.com`) or wildcard
//! (`*.example.com`), with an optional path: exact (`/api`), a subtree
//! (`/api/*`) or a prefix (`/api*`). Hostnames compare without case, as
//! requests match them; paths compare exactly.

/// Splits a route into its hostname and optional path.
pub fn split_route(route: &str) -> (&str, Option<&str>) {
    match route.find('/') {
        Some(idx) => (&route[..idx], Some(&route[idx..])),
        None => (route, None),
    }
}

/// Whether a request for `hostname` and `path` matches `pattern`.
pub fn route_matches(pattern: &str, hostname: &str, path: &str) -> bool {
    let (pattern_host, pattern_path) = split_route(pattern);
    if !hostname_matches(pattern_host, hostname) {
        return false;
    }
    pattern_path.is_none_or(|pattern_path| path_matches(pattern_path, path))
}

/// Whether some request could match both route patterns.
pub fn routes_overlap(a: &str, b: &str) -> bool {
    let (a_host, a_path) = split_route(a);
    let (b_host, b_path) = split_route(b);
    if !host_patterns_overlap(a_host, b_host) {
        return false;
    }
    match (a_path, b_path) {
        (Some(a), Some(b)) => path_patterns_overlap(a, b),
        // A route without a path takes every path of its hosts.
        _ => true,
    }
}

fn hostname_matches(pattern: &str, hostname: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => under_wildcard(suffix, hostname),
        None => pattern.eq_ignore_ascii_case(hostname),
    }
}

/// Whether `host` is a subdomain of `suffix`: `*.example.com` takes
/// `api.example.com` but not `example.com`.
fn under_wildcard(suffix: &str, host: &str) -> bool {
    host.len() > suffix.len() + 1
        && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
        && host[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn host_patterns_overlap(a: &str, b: &str) -> bool {
    match (a.strip_prefix("*."), b.strip_prefix("*.")) {
        (None, None) => a.eq_ignore_ascii_case(b),
        (Some(suffix), None) => under_wildcard(suffix, b),
        (None, Some(suffix)) => under_wildcard(suffix, a),
        (Some(a), Some(b)) => {
            a.eq_ignore_ascii_case(b) || under_wildcard(b, a) || under_wildcard(a, b)
        }
    }
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        path.starts_with(prefix)
            && (path.len() == prefix.len() || path[prefix.len()..].starts_with('/'))
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        path.starts_with(prefix)
    } else {
        normalize_exact_path(pattern) == normalize_exact_path(path)
    }
}

fn normalize_exact_path(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

/// Representative requests from each side are enough: every pattern kind
/// matches its own representatives, and a subtree or prefix pattern that
/// overlaps another contains one of the other's representatives.
fn path_patterns_overlap(a: &str, b: &str) -> bool {
    representative_paths(a)
        .iter()
        .any(|candidate| path_matches(b, candidate))
        || representative_paths(b)
            .iter()
            .any(|candidate| path_matches(a, candidate))
}

fn representative_paths(pattern: &str) -> Vec<String> {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        let child = if prefix == "/" {
            "/x".to_string()
        } else {
            format!("{prefix}/x")
        };
        vec![prefix.to_string(), child]
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        vec![prefix.to_string(), format!("{prefix}x")]
    } else {
        vec![pattern.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_match_hosts_without_case_and_paths_exactly() {
        assert!(route_matches("api.example.com", "API.Example.com", "/"));
        assert!(!route_matches("api.example.com", "www.example.com", "/"));
        assert!(route_matches("*.example.com", "Shop.EXAMPLE.com", "/"));
        assert!(!route_matches("*.example.com", "example.com", "/"));
        assert!(route_matches("example.com/api/*", "example.com", "/api/v1"));
        assert!(route_matches("example.com/api/*", "example.com", "/api"));
        assert!(!route_matches("example.com/api/*", "example.com", "/apix"));
        assert!(route_matches("example.com/api*", "example.com", "/apix"));
        assert!(!route_matches("example.com/API", "example.com", "/api"));
    }

    #[test]
    fn overlapping_routes_share_a_request() {
        assert!(routes_overlap("api.example.com", "API.EXAMPLE.COM"));
        assert!(!routes_overlap("api.example.com", "www.example.com"));
        assert!(routes_overlap("*.example.com", "shop.example.com"));
        assert!(routes_overlap("*.EXAMPLE.com", "shop.example.com"));
        assert!(!routes_overlap("*.example.com", "example.com"));
        assert!(routes_overlap("*.example.com", "*.api.example.com"));
        assert!(!routes_overlap("*.example.com", "*.example.org"));
        assert!(routes_overlap("shop.example.com", "shop.example.com/api/*"));
        assert!(routes_overlap("example.com/api/*", "example.com/api/v1/*"));
        assert!(!routes_overlap("example.com/api/*", "example.com/admin/*"));
        assert!(routes_overlap("example.com/api", "example.com/api/"));
        assert!(!routes_overlap("example.com/api", "example.com/API"));
    }
}
