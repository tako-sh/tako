use super::*;
use std::time::{Duration, Instant};

fn route(app: &str, pattern: &str) -> RouteEntry {
    RouteEntry {
        app: app.to_string(),
        pattern: pattern.to_string(),
        source_ip: tako_core::SourceIpMode::Auto,
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

#[test]
fn test_route_table_finds_app_for_certificate_domain() {
    let mut table = RouteTable::default();
    table.set_app_routes("api".to_string(), vec!["*.example.com/admin/*".to_string()]);

    assert_eq!(
        table.app_for_route_domain("*.EXAMPLE.com").as_deref(),
        Some("api")
    );
    assert_eq!(table.app_for_route_domain("api.example.com"), None);
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
    assert_eq!(matched.app.as_ref(), "web");
    assert_eq!(matched.path.as_deref(), Some("/tanstack-start/*"));
    assert_eq!(matched.source_ip, tako_core::SourceIpMode::Auto);
}

#[test]
fn route_table_select_with_route_reuses_compiled_route_strings() {
    let mut table = RouteTable::default();
    table.set_app_routes(
        "web".to_string(),
        vec!["example.com/tanstack-start/*".to_string()],
    );

    let first = table
        .select_with_route("example.com", "/tanstack-start/assets/main.js")
        .expect("expected first matching route");
    let second = table
        .select_with_route("example.com", "/tanstack-start/assets/main.css")
        .expect("expected second matching route");

    assert_eq!(first.app.as_ref(), "web");
    assert_eq!(
        first.path.as_ref().map(|path| path.as_ref()),
        Some("/tanstack-start/*")
    );
    assert!(std::sync::Arc::ptr_eq(&first.app, &second.app));
    assert!(std::sync::Arc::ptr_eq(
        first.path.as_ref().expect("first route path"),
        second.path.as_ref().expect("second route path")
    ));
}

#[test]
fn route_table_preserves_per_app_source_ip_mode() {
    let mut table = RouteTable::default();
    table.set_app_routes_with_source_ip(
        "web".to_string(),
        vec!["example.com".to_string()],
        tako_core::SourceIpMode::CloudflareProxy,
    );

    let matched = table
        .select_with_route("example.com", "/")
        .expect("expected matching route");

    assert_eq!(matched.source_ip, tako_core::SourceIpMode::CloudflareProxy);
}

#[test]
fn route_table_reports_when_cloudflare_ranges_are_needed() {
    let mut table = RouteTable::default();
    assert!(!table.needs_cloudflare_ip_ranges());

    table.set_app_routes_with_source_ip(
        "web".to_string(),
        vec!["example.com".to_string()],
        tako_core::SourceIpMode::TrustedProxy,
    );
    assert!(!table.needs_cloudflare_ip_ranges());

    table.set_app_routes_with_source_ip(
        "api".to_string(),
        vec!["api.example.com".to_string()],
        tako_core::SourceIpMode::Auto,
    );
    assert!(table.needs_cloudflare_ip_ranges());
}
