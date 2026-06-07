use super::*;

#[test]
fn test_should_redirect_http_request_when_http_and_enabled() {
    assert!(should_redirect_http_request(false, true));
}

#[test]
fn https_redirect_host_replaces_public_http_port() {
    assert_eq!(
        https_redirect_host("example.com:8080", 8443),
        "example.com:8443"
    );
    assert_eq!(https_redirect_host("example.com:8080", 443), "example.com");
    assert_eq!(
        https_redirect_host("[fd7a:115c:a1e0::1]:8080", 8443),
        "[fd7a:115c:a1e0::1]:8443"
    );
}

#[test]
fn test_should_not_redirect_http_request_when_already_https() {
    assert!(!should_redirect_http_request(true, true));
}

#[test]
fn test_should_not_redirect_http_request_when_disabled() {
    assert!(!should_redirect_http_request(false, false));
}

#[test]
fn test_should_not_redirect_http_request_when_forwarded_proto_is_https() {
    assert!(is_request_forwarded_https(Some("https"), None));
    assert!(!should_redirect_http_request(true, true));
}

#[test]
fn test_should_not_redirect_http_request_when_forwarded_header_proto_is_https() {
    assert!(is_request_forwarded_https(
        None,
        Some("for=192.0.2.60;proto=https;by=203.0.113.43")
    ));
    assert!(!should_redirect_http_request(true, true));
}

#[test]
fn test_effective_request_https_prefers_transport_tls() {
    let cloudflare_ips = CloudflareIpRanges::default();
    let trusted_proxy = TrustedProxyConfig::default();

    assert!(is_effective_request_https(
        true,
        "api.example.com",
        None,
        None,
        None,
        forwarded_header_trust(None, &cloudflare_ips, &trusted_proxy)
    ));
}

#[test]
fn test_effective_request_https_uses_forwarded_https_when_transport_is_http() {
    let cloudflare_ips = CloudflareIpRanges::default();
    let trusted_proxy =
        TrustedProxyConfig::from_raw(false, &["10.0.0.0/8".to_string()], &[]).unwrap();

    assert!(is_effective_request_https(
        false,
        "api.example.com",
        None,
        Some("https"),
        None,
        forwarded_header_trust(Some("10.1.2.3"), &cloudflare_ips, &trusted_proxy)
    ));
    assert!(is_effective_request_https(
        false,
        "api.example.com",
        None,
        None,
        Some("for=192.0.2.60;proto=https"),
        forwarded_header_trust(Some("10.1.2.3"), &cloudflare_ips, &trusted_proxy)
    ));
    assert!(!is_effective_request_https(
        false,
        "api.example.com",
        None,
        Some("http"),
        None,
        forwarded_header_trust(Some("10.1.2.3"), &cloudflare_ips, &trusted_proxy)
    ));
}

#[test]
fn test_effective_request_https_ignores_forwarded_https_from_untrusted_peer() {
    let cloudflare_ips = CloudflareIpRanges::default();
    let trusted_proxy = TrustedProxyConfig::default();

    assert!(!is_effective_request_https(
        false,
        "api.example.com",
        None,
        Some("https"),
        None,
        forwarded_header_trust(Some("198.51.100.10"), &cloudflare_ips, &trusted_proxy)
    ));
    assert!(!is_effective_request_https(
        false,
        "api.example.com",
        None,
        None,
        Some("for=192.0.2.60;proto=https"),
        forwarded_header_trust(Some("198.51.100.10"), &cloudflare_ips, &trusted_proxy)
    ));
}

#[test]
fn test_effective_request_https_trusts_forwarded_https_from_cloudflare_and_loopback() {
    let cloudflare_ips = CloudflareIpRanges::from_test_cidrs(&["198.51.100.0/24"]);
    let trusted_proxy = TrustedProxyConfig::default();

    assert!(is_effective_request_https(
        false,
        "api.example.com",
        None,
        Some("https"),
        None,
        forwarded_header_trust(Some("198.51.100.10"), &cloudflare_ips, &trusted_proxy)
    ));

    let cloudflare_ips = CloudflareIpRanges::default();
    assert!(is_effective_request_https(
        false,
        "api.example.com",
        None,
        Some("https"),
        None,
        forwarded_header_trust(Some("127.0.0.1"), &cloudflare_ips, &trusted_proxy)
    ));
}

fn forwarded_header_trust<'a>(
    peer_ip: Option<&str>,
    cloudflare_ips: &'a CloudflareIpRanges,
    trusted_proxy: &'a TrustedProxyConfig,
) -> ForwardedHeaderTrust<'a> {
    ForwardedHeaderTrust {
        peer_ip: peer_ip.map(|ip| ip.parse().unwrap()),
        cloudflare_ips,
        trusted_proxy,
    }
}

#[test]
fn test_private_local_forwarded_request_without_proto_is_treated_as_https() {
    let inferred_https = should_assume_forwarded_private_request_https(
        "test-app.orb.local",
        Some("127.0.0.1"),
        None,
        None,
    );
    assert!(inferred_https);
}

#[test]
fn test_private_local_forwarded_request_with_proto_is_not_inferred() {
    assert!(!should_assume_forwarded_private_request_https(
        "test-app.orb.local",
        Some("127.0.0.1"),
        Some("http"),
        None,
    ));
    assert!(!should_assume_forwarded_private_request_https(
        "test-app.orb.local",
        None,
        None,
        Some("for=127.0.0.1;proto=https"),
    ));
}

#[test]
fn test_public_forwarded_request_without_proto_is_not_inferred() {
    assert!(!should_assume_forwarded_private_request_https(
        "api.example.com",
        Some("127.0.0.1"),
        None,
        None,
    ));
}

#[test]
fn test_forwarded_header_has_proto_detects_presence() {
    assert!(forwarded_header_has_proto("for=192.0.2.60;proto=https"));
    assert!(forwarded_header_has_proto(
        r#"for=192.0.2.60;proto="http";by=203.0.113.43"#
    ));
    assert!(!forwarded_header_has_proto(
        "for=192.0.2.60;by=203.0.113.43"
    ));
    assert!(!forwarded_header_has_proto(r#"for=192.0.2.60;proto="""#));
}

#[test]
fn test_x_forwarded_proto_parsing_handles_case_and_commas() {
    assert!(x_forwarded_proto_is_https("HTTPS"));
    assert!(x_forwarded_proto_is_https("https, http"));
    assert!(!x_forwarded_proto_is_https("http, https"));
}

#[test]
fn test_forwarded_header_parsing_handles_quotes_and_multiple_entries() {
    assert!(forwarded_header_proto_is_https(
        r#"for=192.0.2.60;proto="https";by=203.0.113.43"#
    ));
    assert!(forwarded_header_proto_is_https(
        "for=192.0.2.60;proto=http,for=198.51.100.17;proto=https"
    ));
    assert!(!forwarded_header_proto_is_https(
        "for=192.0.2.60;proto=http"
    ));
}
