//! Server-side routing: match incoming requests (Host + path) to an app.
//!
//! This is intentionally pure logic (no Pingora types) to keep it easy to test.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    pub app: String,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledRouteEntry {
    pub app: String,
    pub host: String,
    pub path: Option<String>,
    pub specificity: (u8, usize, u8),
}

#[derive(Debug, Default, Clone)]
pub struct RouteTable {
    app_routes: std::collections::HashMap<String, Vec<String>>,
    compiled: Vec<CompiledRouteEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedRoute {
    pub app: String,
    pub path: Option<String>,
}

impl RouteTable {
    pub fn set_app_routes(&mut self, app: String, routes: Vec<String>) {
        self.app_routes.insert(app, routes);
        self.rebuild();
    }

    pub fn remove_app_routes(&mut self, app: &str) {
        self.app_routes.remove(app);
        self.rebuild();
    }

    pub fn routes_for_app(&self, app: &str) -> Vec<String> {
        self.app_routes.get(app).cloned().unwrap_or_default()
    }

    pub fn select(&self, host: &str, path: &str) -> Option<String> {
        self.select_with_route(host, path)
            .map(|selected| selected.app)
    }

    pub fn select_with_route(&self, host: &str, path: &str) -> Option<SelectedRoute> {
        select_route_for_request_compiled(&self.compiled, host, path)
    }

    fn rebuild(&mut self) {
        let mut entries = Vec::new();

        for (app, patterns) in &self.app_routes {
            for pattern in patterns {
                if pattern.is_empty() {
                    continue;
                }
                entries.push(RouteEntry {
                    app: app.clone(),
                    pattern: pattern.clone(),
                });
            }
        }

        self.compiled = compile_routes(&entries);
    }
}

pub fn compile_routes(routes: &[RouteEntry]) -> Vec<CompiledRouteEntry> {
    let mut compiled = Vec::with_capacity(routes.len());
    for entry in routes {
        if entry.pattern.is_empty() {
            continue;
        }

        let (pattern_host, pattern_path) = split_route(&entry.pattern);
        compiled.push(CompiledRouteEntry {
            app: entry.app.clone(),
            host: pattern_host.to_string(),
            path: pattern_path.map(|p| p.to_string()),
            specificity: route_specificity(&entry.pattern),
        });
    }

    // Most specific first. Keep stable order for ties.
    compiled.sort_by(|a, b| b.specificity.cmp(&a.specificity));
    compiled
}

#[cfg(test)]
pub fn select_app_for_request_compiled(
    routes: &[CompiledRouteEntry],
    host: &str,
    path: &str,
) -> Option<String> {
    select_route_for_request_compiled(routes, host, path).map(|selected| selected.app)
}

pub fn select_route_for_request_compiled(
    routes: &[CompiledRouteEntry],
    host: &str,
    path: &str,
) -> Option<SelectedRoute> {
    for entry in routes {
        if !hostname_matches(&entry.host, host) {
            continue;
        }
        if let Some(p) = &entry.path
            && !path_matches(p, path)
        {
            continue;
        }
        return Some(SelectedRoute {
            app: entry.app.clone(),
            path: entry.path.clone(),
        });
    }
    None
}

/// Select the best matching app for a request (uncompiled reference implementation, tests only).
#[cfg(test)]
fn select_app_for_request(routes: &[RouteEntry], host: &str, path: &str) -> Option<String> {
    let mut best: Option<(&RouteEntry, (u8, usize, u8))> = None;

    for entry in routes {
        if !route_matches(&entry.pattern, host, path) {
            continue;
        }

        let spec = route_specificity(&entry.pattern);
        match best {
            None => best = Some((entry, spec)),
            Some((_, best_spec)) => {
                if spec > best_spec {
                    best = Some((entry, spec));
                }
            }
        }
    }

    best.map(|(e, _)| e.app.clone())
}

#[cfg(test)]
fn route_matches(pattern: &str, host: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let (pattern_host, pattern_path) = split_route(pattern);
    if !hostname_matches(pattern_host, host) {
        return false;
    }
    match pattern_path {
        None => true,
        Some(p) => path_matches(p, path),
    }
}

fn route_specificity(pattern: &str) -> (u8, usize, u8) {
    if pattern.is_empty() {
        return (0, 0, 0);
    }
    let (pattern_host, pattern_path) = split_route(pattern);

    let host_score: u8 = if pattern_host.starts_with("*.") { 1 } else { 2 };

    let (path_len, exact_bonus) = match pattern_path {
        None => (0, 0),
        Some(p) => {
            if let Some(prefix) = p.strip_suffix("/*") {
                (prefix.len(), 0)
            } else if p.ends_with('*') {
                let prefix = &p[..p.len().saturating_sub(1)];
                (prefix.len(), 0)
            } else {
                (normalize_exact_path(p).len(), 1)
            }
        }
    };

    (host_score, path_len, exact_bonus)
}

fn split_route(route: &str) -> (&str, Option<&str>) {
    match route.find('/') {
        Some(idx) => (&route[..idx], Some(&route[idx..])),
        None => (route, None),
    }
}

fn hostname_matches(pattern: &str, hostname: &str) -> bool {
    // RFC 7230 §2.7.1: host is case-insensitive
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // *.example.com should not match example.com
        if hostname.eq_ignore_ascii_case(suffix) {
            return false;
        }
        // Check hostname ends with ".{suffix}" without allocating
        hostname.len() > suffix.len()
            && hostname.as_bytes()[hostname.len() - suffix.len() - 1] == b'.'
            && hostname[hostname.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    } else {
        pattern.eq_ignore_ascii_case(hostname)
    }
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        path.starts_with(prefix)
            && (path.len() == prefix.len() || path[prefix.len()..].starts_with('/'))
    } else if pattern.ends_with('*') {
        let prefix = &pattern[..pattern.len().saturating_sub(1)];
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn route(app: &str, pattern: &str) -> RouteEntry {
        RouteEntry {
            app: app.to_string(),
            pattern: pattern.to_string(),
        }
    }

    fn compiled(routes: &[RouteEntry]) -> Vec<CompiledRouteEntry> {
        compile_routes(routes)
    }

    // ===========================================
    // Basic matching tests
    // ===========================================

    #[test]
    fn test_select_app_exact_host_beats_wildcard() {
        let routes = vec![
            route("wild", "*.example.com"),
            route("exact", "api.example.com"),
        ];
        assert_eq!(
            select_app_for_request(&routes, "api.example.com", "/"),
            Some("exact".to_string())
        );
        assert_eq!(
            select_app_for_request_compiled(&compiled(&routes), "api.example.com", "/"),
            Some("exact".to_string())
        );
    }

    #[test]
    fn test_select_app_longer_path_beats_shorter() {
        let routes = vec![
            route("short", "example.com/api/*"),
            route("long", "example.com/api/v1/*"),
        ];
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/api/v1/users"),
            Some("long".to_string())
        );
        assert_eq!(
            select_app_for_request_compiled(&compiled(&routes), "example.com", "/api/v1/users"),
            Some("long".to_string())
        );
    }

    #[test]
    fn test_select_app_no_match() {
        let routes = vec![route("a", "api.example.com")];
        assert_eq!(select_app_for_request(&routes, "example.com", "/"), None);
        assert_eq!(
            select_app_for_request_compiled(&compiled(&routes), "example.com", "/"),
            None
        );
    }

    // ===========================================
    // Empty pattern tests
    // ===========================================

    #[test]
    fn test_empty_pattern_matches_nothing() {
        let routes = vec![route("catchall", "")];
        assert_eq!(
            select_app_for_request(&routes, "any.domain.com", "/any/path"),
            None
        );
    }

    #[test]
    fn test_specific_pattern_ignores_empty_pattern() {
        let routes = vec![route("catchall", ""), route("specific", "api.example.com")];
        assert_eq!(
            select_app_for_request(&routes, "api.example.com", "/"),
            Some("specific".to_string())
        );
        assert_eq!(select_app_for_request(&routes, "other.com", "/"), None);

        assert_eq!(
            select_app_for_request_compiled(&compiled(&routes), "api.example.com", "/"),
            Some("specific".to_string())
        );
    }

    #[test]
    fn test_empty_routes_returns_none() {
        let routes: Vec<RouteEntry> = vec![];
        assert_eq!(select_app_for_request(&routes, "example.com", "/"), None);
    }

    #[test]
    fn test_route_table_single_app_without_routes_matches_nothing() {
        let mut table = RouteTable::default();
        table.set_app_routes("app".to_string(), vec![]);

        assert_eq!(table.select("unknown.example.com", "/any/path"), None);
    }

    #[test]
    fn test_route_table_does_not_use_no_route_app_as_catchall_fallback() {
        let mut table = RouteTable::default();
        table.set_app_routes("fallback".to_string(), vec![]);
        table.set_app_routes("api".to_string(), vec!["api.example.com".to_string()]);

        assert_eq!(table.select("other.example.com", "/"), None);
        assert_eq!(
            table.select("api.example.com", "/"),
            Some("api".to_string())
        );
    }

    #[test]
    fn perf_smoke_compiled_route_selection_large_table() {
        let route_count = 500usize;
        let routes: Vec<RouteEntry> = (0..route_count)
            .map(|idx| {
                route(
                    &format!("app-{idx}"),
                    &format!("app-{idx}.example.com/api/*"),
                )
            })
            .collect();
        let compiled = compile_routes(&routes);
        let hosts: Vec<String> = (0..route_count)
            .map(|idx| format!("app-{idx}.example.com"))
            .collect();
        let expected_apps: Vec<String> = (0..route_count).map(|idx| format!("app-{idx}")).collect();

        let start = Instant::now();
        for iteration in 0..50_000usize {
            let idx = iteration % route_count;
            let selected = select_app_for_request_compiled(&compiled, &hosts[idx], "/api/ping");
            assert_eq!(selected, Some(expected_apps[idx].clone()));
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "compiled route selection perf smoke threshold exceeded: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn test_route_table_ignores_multiple_no_route_apps() {
        let mut table = RouteTable::default();
        table.set_app_routes("fallback-a".to_string(), vec![]);
        table.set_app_routes("fallback-b".to_string(), vec![]);
        table.set_app_routes("api".to_string(), vec!["api.example.com".to_string()]);

        assert_eq!(table.select("other.example.com", "/"), None);
        assert_eq!(
            table.select("api.example.com", "/"),
            Some("api".to_string())
        );
    }

    #[test]
    fn test_route_table_remove_app_routes() {
        let mut table = RouteTable::default();
        table.set_app_routes("api".to_string(), vec!["api.example.com".to_string()]);
        table.set_app_routes("web".to_string(), vec!["example.com".to_string()]);

        table.remove_app_routes("api");

        assert_eq!(table.routes_for_app("api"), Vec::<String>::new());
        assert_eq!(
            table.select("api.example.com", "/"),
            None,
            "removed app routes should no longer match"
        );
        assert_eq!(
            table.select("example.com", "/"),
            Some("web".to_string()),
            "other apps should remain routable"
        );
    }

    // ===========================================
    // Hostname matching tests
    // ===========================================

    #[test]
    fn test_hostname_exact_match() {
        assert!(hostname_matches("api.example.com", "api.example.com"));
        assert!(!hostname_matches("api.example.com", "www.example.com"));
        assert!(!hostname_matches("api.example.com", "example.com"));
    }

    #[test]
    fn test_hostname_wildcard_match() {
        assert!(hostname_matches("*.example.com", "api.example.com"));
        assert!(hostname_matches("*.example.com", "www.example.com"));
        assert!(hostname_matches("*.example.com", "deep.sub.example.com"));
    }

    #[test]
    fn test_hostname_wildcard_does_not_match_apex() {
        // *.example.com should NOT match example.com
        assert!(!hostname_matches("*.example.com", "example.com"));
    }

    #[test]
    fn test_hostname_wildcard_requires_subdomain() {
        // *.example.com should not match otherexample.com
        assert!(!hostname_matches("*.example.com", "otherexample.com"));
        assert!(!hostname_matches("*.example.com", "fakeexample.com"));
    }

    #[test]
    fn test_hostname_matching_is_case_insensitive() {
        // RFC 7230 §2.7.1: host is case-insensitive
        assert!(hostname_matches("api.example.com", "API.Example.Com"));
        assert!(hostname_matches("API.EXAMPLE.COM", "api.example.com"));
        assert!(hostname_matches("Api.Example.Com", "api.example.com"));

        // Wildcard patterns are also case-insensitive
        assert!(hostname_matches("*.example.com", "API.Example.Com"));
        assert!(hostname_matches("*.EXAMPLE.COM", "api.example.com"));

        // Wildcard apex exclusion still works with mixed case
        assert!(!hostname_matches("*.example.com", "Example.Com"));
        assert!(!hostname_matches("*.EXAMPLE.COM", "example.com"));
    }

    #[test]
    fn test_case_insensitive_routing_end_to_end() {
        let routes = vec![
            route("api", "api.example.com"),
            route("catchall", "*.example.com"),
        ];
        assert_eq!(
            select_app_for_request(&routes, "API.Example.Com", "/"),
            Some("api".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "Blog.Example.Com", "/"),
            Some("catchall".to_string())
        );

        let c = compiled(&routes);
        assert_eq!(
            select_app_for_request_compiled(&c, "API.EXAMPLE.COM", "/"),
            Some("api".to_string())
        );
        assert_eq!(
            select_app_for_request_compiled(&c, "BLOG.EXAMPLE.COM", "/"),
            Some("catchall".to_string())
        );
    }

    // ===========================================
    // Path matching tests
    // ===========================================

    #[test]
    fn test_path_exact_match() {
        assert!(path_matches("/api/users", "/api/users"));
        assert!(path_matches("/api/users", "/api/users/"));
        assert!(path_matches("/api/users/", "/api/users"));
        assert!(path_matches("/api/users/", "/api/users/"));
        assert!(!path_matches("/api/users", "/api/users/123"));
    }

    #[test]
    fn test_path_prefix_with_slash_star() {
        // /api/* matches /api/anything but requires the path separator
        assert!(path_matches("/api/*", "/api/users"));
        assert!(path_matches("/api/*", "/api/users/123"));
        assert!(path_matches("/api/*", "/api/"));
        // Should match exact prefix too
        assert!(path_matches("/api/*", "/api"));
        // Should not match /apifoo (no separator)
        assert!(!path_matches("/api/*", "/apifoo"));
    }

    #[test]
    fn test_path_prefix_with_star() {
        // /api* matches anything starting with /api
        assert!(path_matches("/api*", "/api"));
        assert!(path_matches("/api*", "/api/"));
        assert!(path_matches("/api*", "/api/users"));
        assert!(path_matches("/api*", "/apiv2")); // Note: this matches unlike /*
    }

    #[test]
    fn test_path_none_matches_all() {
        let routes = vec![route("app", "example.com")];
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/any/path"),
            Some("app".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/"),
            Some("app".to_string())
        );
    }

    #[test]
    fn test_host_only_and_host_slash_star_match_equivalently() {
        for candidate_path in ["/", "/api", "/api/v1/users"] {
            assert!(
                route_matches("example.com", "example.com", candidate_path),
                "host-only route should match path {candidate_path}"
            );
            assert!(
                route_matches("example.com/*", "example.com", candidate_path),
                "host/* route should match path {candidate_path}"
            );
        }

        assert_eq!(
            route_specificity("example.com"),
            route_specificity("example.com/*")
        );
    }

    // ===========================================
    // Route specificity tests
    // ===========================================

    #[test]
    fn test_specificity_exact_path_beats_wildcard_path() {
        let routes = vec![
            route("wildcard", "example.com/api/*"),
            route("exact", "example.com/api/users"),
        ];
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/api/users"),
            Some("exact".to_string())
        );
    }

    #[test]
    fn test_specificity_host_beats_path_length() {
        let routes = vec![
            route("wildcard_host", "*.example.com/api/*"),
            route("exact_host", "api.example.com/*"),
        ];
        // Exact host should win even with shorter path pattern
        assert_eq!(
            select_app_for_request(&routes, "api.example.com", "/api/v1/users"),
            Some("exact_host".to_string())
        );
    }

    #[test]
    fn test_specificity_scores() {
        // Empty pattern
        assert_eq!(route_specificity(""), (0, 0, 0));

        // Host only (exact)
        assert_eq!(route_specificity("example.com"), (2, 0, 0));

        // Host only (wildcard)
        assert_eq!(route_specificity("*.example.com"), (1, 0, 0));

        // Exact host + exact path
        assert_eq!(route_specificity("example.com/api"), (2, 4, 1));

        // Exact host + wildcard path
        assert_eq!(route_specificity("example.com/api/*"), (2, 4, 0));
        assert_eq!(route_specificity("example.com/api*"), (2, 4, 0));

        // Wildcard host + exact path
        assert_eq!(route_specificity("*.example.com/api"), (1, 4, 1));
    }

    // ===========================================
    // Split route tests
    // ===========================================

    #[test]
    fn test_split_route_host_only() {
        assert_eq!(split_route("example.com"), ("example.com", None));
        assert_eq!(split_route("*.example.com"), ("*.example.com", None));
    }

    #[test]
    fn test_split_route_with_path() {
        assert_eq!(
            split_route("example.com/api"),
            ("example.com", Some("/api"))
        );
        assert_eq!(
            split_route("example.com/api/v1"),
            ("example.com", Some("/api/v1"))
        );
    }

    // ===========================================
    // Complex scenarios
    // ===========================================

    #[test]
    fn test_multiple_apps_different_paths() {
        let routes = vec![
            route("api", "example.com/api/*"),
            route("admin", "example.com/admin/*"),
            route("web", "example.com/*"),
        ];

        assert_eq!(
            select_app_for_request(&routes, "example.com", "/api/users"),
            Some("api".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/admin/dashboard"),
            Some("admin".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/about"),
            Some("web".to_string())
        );
    }

    #[test]
    fn test_multiple_apps_different_subdomains() {
        let routes = vec![
            route("api", "api.example.com"),
            route("admin", "admin.example.com"),
            route("catchall", "*.example.com"),
        ];

        assert_eq!(
            select_app_for_request(&routes, "api.example.com", "/"),
            Some("api".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "admin.example.com", "/"),
            Some("admin".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "blog.example.com", "/"),
            Some("catchall".to_string())
        );
    }

    #[test]
    fn test_first_match_wins_on_equal_specificity() {
        let routes = vec![
            route("first", "example.com/api"),
            route("second", "example.com/api"),
        ];
        // When specificity is equal, first route should win
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/api"),
            Some("first".to_string())
        );
    }

    #[test]
    fn test_complex_multi_level_routing() {
        let routes = vec![
            route("api-v2", "api.example.com/v2/*"),
            route("api-v1", "api.example.com/v1/*"),
            route("api-fallback", "api.example.com/*"),
            route("web", "www.example.com/*"),
            route("wildcard", "*.example.com"),
        ];

        // Most specific matches
        assert_eq!(
            select_app_for_request(&routes, "api.example.com", "/v2/users"),
            Some("api-v2".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "api.example.com", "/v1/users"),
            Some("api-v1".to_string())
        );
        // Fallback within api subdomain
        assert_eq!(
            select_app_for_request(&routes, "api.example.com", "/health"),
            Some("api-fallback".to_string())
        );
        // www subdomain
        assert_eq!(
            select_app_for_request(&routes, "www.example.com", "/about"),
            Some("web".to_string())
        );
        // Other subdomains hit wildcard
        assert_eq!(
            select_app_for_request(&routes, "blog.example.com", "/post/123"),
            Some("wildcard".to_string())
        );
        // Completely different domain has no route
        assert_eq!(select_app_for_request(&routes, "other.com", "/"), None);
    }

    // ===========================================
    // Edge cases
    // ===========================================

    #[test]
    fn test_trailing_slash_in_path() {
        let routes = vec![route("app", "example.com/api")];
        // Exact path routes normalize trailing slash, so /api and /api/ are equivalent.
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/api"),
            Some("app".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/api/"),
            Some("app".to_string())
        );

        let routes_with_slash = vec![route("app", "example.com/api/")];
        assert_eq!(
            select_app_for_request(&routes_with_slash, "example.com", "/api"),
            Some("app".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes_with_slash, "example.com", "/api/"),
            Some("app".to_string())
        );
    }

    #[test]
    fn test_root_path() {
        let routes = vec![route("app", "example.com/")];
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/"),
            Some("app".to_string())
        );
        assert_eq!(
            select_app_for_request(&routes, "example.com", "/other"),
            None
        );
    }

    #[test]
    fn test_case_sensitivity() {
        let routes = vec![route("app", "Example.Com/API")];
        // Routing is case-sensitive
        assert_eq!(
            select_app_for_request(&routes, "Example.Com", "/API"),
            Some("app".to_string())
        );
        assert_eq!(select_app_for_request(&routes, "example.com", "/api"), None);
    }

    #[test]
    fn test_route_table_select_with_route_returns_matched_path_pattern() {
        let mut table = RouteTable::default();
        table.set_app_routes(
            "web".to_string(),
            vec!["example.com/tanstack-start/*".to_string()],
        );

        let matched = table
            .select_with_route("example.com", "/tanstack-start/assets/main.js")
            .expect("expected matching route");
        assert_eq!(matched.app, "web");
        assert_eq!(matched.path, Some("/tanstack-start/*".to_string()));
    }
}
