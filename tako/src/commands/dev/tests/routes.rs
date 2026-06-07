use super::*;

#[test]
fn prefers_local_url_when_80_443_forwarding_is_detected() {
    let url = preferred_public_url(
        "bun-example.test",
        "https://bun-example.test:47831/",
        47831,
        443,
    );
    assert_eq!(url, "https://bun-example.test/");
}

#[test]
fn prefers_daemon_url_when_display_and_listen_ports_match() {
    let url = preferred_public_url(
        "bun-example.test",
        "https://bun-example.test:47831/",
        47831,
        47831,
    );
    assert_eq!(url, "https://bun-example.test:47831/");
}

#[test]
fn display_routes_always_includes_default() {
    let cfg = TakoToml::default();
    let routes = compute_display_routes(&cfg, "app.test", None);
    assert_eq!(routes, vec!["app.test"]);
}

#[test]
fn display_routes_omit_default_when_explicit_routes_configured() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"app.test/bun\", \"*.app.test\"]\n")
        .unwrap();
    let routes = compute_display_routes(&cfg, "app.test", None);
    assert_eq!(routes, vec!["app.test/bun", "*.app.test"]);
}

#[test]
fn display_routes_use_user_configured_default_as_sole_route() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"app.test\"]\n").unwrap();
    let routes = compute_display_routes(&cfg, "app.test", None);
    assert_eq!(routes, vec!["app.test"]);
}

#[test]
fn display_routes_rewrite_wildcard_for_variant() {
    let cfg = TakoToml::parse(
        "[envs.development]\nroutes = [\"some-app.test/bun\", \"*.example.test\"]\n",
    )
    .unwrap();
    let routes = compute_display_routes(&cfg, "example-foo.test", Some("example.test"));
    assert_eq!(routes, vec!["some-app.test/bun", "*.example-foo.test",]);
}

#[test]
fn display_routes_variant_rewrites_base_domain_in_user_routes() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"example.test\"]\n").unwrap();
    let routes = compute_display_routes(&cfg, "example-foo.test", Some("example.test"));
    assert_eq!(routes, vec!["example-foo.test"]);
}

#[test]
fn display_routes_include_default_for_external_only_routes() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"tunnel.example.com\"]\n").unwrap();
    let routes = compute_display_routes(&cfg, "app.test", None);
    assert_eq!(routes, vec!["app.test", "tunnel.example.com"]);
}

#[test]
fn local_https_probe_host_uses_app_test_domain() {
    assert_eq!(
        local_https_probe_host("bun-example.test"),
        "bun-example.test"
    );
}

#[test]
fn falls_back_to_default_host_when_development_routes_are_missing() {
    let cfg = TakoToml::default();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["app.test".to_string()]);
}

#[test]
fn falls_back_to_default_host_when_development_routes_are_empty() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = []\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["app.test".to_string()]);
}

#[test]
fn explicit_routes_omit_default_host() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"api.app.test\"]\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["api.app.test"]);
}

#[test]
fn external_only_routes_keep_default_host() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"tunnel.example.com\"]\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["app.test", "tunnel.example.com"]);
}

#[test]
fn external_routes_are_additive_to_explicit_dev_routes() {
    let cfg = TakoToml::parse(
        "[envs.development]\nroutes = [\"api.app.test\", \"tunnel.example.com\"]\n",
    )
    .unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["api.app.test", "tunnel.example.com"]);
}

#[test]
fn wildcard_only_routes_omit_default_host() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"*.app.test\"]\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["*.app.test"]);
}

#[test]
fn user_default_host_as_sole_route_passes_through() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"app.test\"]\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["app.test"]);
}

#[test]
fn dev_hosts_rewrite_wildcard_for_variant() {
    let cfg = TakoToml::parse(
        "[envs.development]\nroutes = [\"some-app.test/bun\", \"*.example.test\"]\n",
    )
    .unwrap();
    let hosts = compute_dev_hosts(
        "example-foo",
        &cfg,
        "example-foo.test",
        Some("example.test"),
    )
    .unwrap();
    assert_eq!(hosts, vec!["some-app.test/bun", "*.example-foo.test",]);
}

#[test]
fn dev_hosts_now_include_paths_and_wildcards() {
    let cfg = TakoToml::parse(
        "[envs.development]\nroutes = [\"app.test\", \"app.test/api\", \"*.app.test\"]\n",
    )
    .unwrap();
    let display = compute_display_routes(&cfg, "app.test", None);
    let routing = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();

    assert_eq!(display, vec!["app.test", "app.test/api", "*.app.test"]);
    assert_eq!(routing, vec!["app.test", "app.test/api", "*.app.test"]);
}

#[test]
fn route_hostname_matches_exact() {
    assert!(route_hostname_matches("app.test", "app.test"));
    assert!(!route_hostname_matches("app.test", "other.test"));
}

#[test]
fn route_hostname_matches_with_path() {
    assert!(route_hostname_matches("app.test/api", "app.test"));
    assert!(!route_hostname_matches("app.test/api", "other.test"));
}

#[test]
fn route_hostname_matches_wildcard() {
    assert!(route_hostname_matches("*.app.test", "foo.app.test"));
    assert!(!route_hostname_matches("*.app.test", "app.test"));
    assert!(!route_hostname_matches("*.app.test", "other.test"));
}
