use super::*;

#[test]
fn trusted_proxy_config_matches_configured_cidrs() {
    let config = TrustedProxyConfig::from_raw(
        true,
        &["127.0.0.1/32".to_string(), "10.0.0.0/8".to_string()],
        &[],
    )
    .unwrap();

    assert!(config.trusts_proxy_ip(&"127.0.0.1".parse().unwrap()));
    assert!(config.trusts_proxy_ip(&"10.1.2.3".parse().unwrap()));
    assert!(!config.trusts_proxy_ip(&"192.0.2.10".parse().unwrap()));
}

#[test]
fn trusted_headers_use_first_configured_valid_client_ip() {
    let config = TrustedProxyConfig::from_raw(
        false,
        &["127.0.0.1/32".to_string()],
        &[
            "cf-connecting-ip".to_string(),
            "x-forwarded-for".to_string(),
        ],
    )
    .unwrap();
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("CF-Connecting-IP", "203.0.113.15")
        .unwrap();
    request
        .insert_header("X-Forwarded-For", "198.51.100.10, 127.0.0.1")
        .unwrap();

    let ip = client_ip_from_trusted_headers(&request, "127.0.0.1".parse().unwrap(), &config);

    assert_eq!(ip, Some("203.0.113.15".parse().unwrap()));
}

#[test]
fn trusted_headers_are_ignored_from_untrusted_peer() {
    let config = TrustedProxyConfig::from_raw(
        false,
        &["127.0.0.1/32".to_string()],
        &["x-forwarded-for".to_string()],
    )
    .unwrap();
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("X-Forwarded-For", "203.0.113.15")
        .unwrap();

    let ip = client_ip_from_trusted_headers(&request, "198.51.100.1".parse().unwrap(), &config);

    assert_eq!(ip, None);
}

#[test]
fn x_forwarded_for_uses_only_leftmost_address() {
    let config = TrustedProxyConfig::from_raw(
        false,
        &["127.0.0.1/32".to_string()],
        &["x-forwarded-for".to_string()],
    )
    .unwrap();
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("X-Forwarded-For", "not-an-ip, 203.0.113.15")
        .unwrap();

    let ip = client_ip_from_trusted_headers(&request, "127.0.0.1".parse().unwrap(), &config);

    assert_eq!(ip, None);
}

#[test]
fn auto_source_ip_uses_cloudflare_header_from_cloudflare_peer() {
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("CF-Connecting-IP", "203.0.113.15")
        .unwrap();
    let cloudflare_ips = CloudflareIpRanges::from_test_cidrs(&["198.51.100.0/24"]);

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "198.51.100.1".parse().unwrap(),
        tako_core::SourceIpMode::Auto,
        &cloudflare_ips,
        &TrustedProxyConfig::default(),
    );

    assert_eq!(
        resolution,
        ClientIpResolution::Accepted("203.0.113.15".parse().unwrap())
    );
}

#[test]
fn auto_source_ip_falls_back_to_direct_peer_when_not_cloudflare() {
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("CF-Connecting-IP", "203.0.113.15")
        .unwrap();
    let cloudflare_ips = CloudflareIpRanges::from_test_cidrs(&["198.51.100.0/24"]);

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "192.0.2.10".parse().unwrap(),
        tako_core::SourceIpMode::Auto,
        &cloudflare_ips,
        &TrustedProxyConfig::default(),
    );

    assert_eq!(
        resolution,
        ClientIpResolution::Accepted("192.0.2.10".parse().unwrap())
    );
}

#[test]
fn strict_cloudflare_source_ip_rejects_non_cloudflare_peer() {
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("CF-Connecting-IP", "203.0.113.15")
        .unwrap();
    let cloudflare_ips = CloudflareIpRanges::from_test_cidrs(&["198.51.100.0/24"]);

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "192.0.2.10".parse().unwrap(),
        tako_core::SourceIpMode::CloudflareProxy,
        &cloudflare_ips,
        &TrustedProxyConfig::default(),
    );

    assert_eq!(resolution, ClientIpResolution::RejectCloudflareProxy);
}

#[test]
fn strict_cloudflare_source_ip_rejects_cloudflare_peer_without_header() {
    let request = RequestHeader::build("GET", b"/", None).expect("build request");
    let cloudflare_ips = CloudflareIpRanges::from_test_cidrs(&["198.51.100.0/24"]);

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "198.51.100.1".parse().unwrap(),
        tako_core::SourceIpMode::CloudflareProxy,
        &cloudflare_ips,
        &TrustedProxyConfig::default(),
    );

    assert_eq!(resolution, ClientIpResolution::RejectCloudflareProxy);
}

#[test]
fn trusted_proxy_source_ip_uses_x_forwarded_for_from_loopback() {
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("X-Forwarded-For", "203.0.113.15, 127.0.0.1")
        .unwrap();

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "127.0.0.1".parse().unwrap(),
        tako_core::SourceIpMode::TrustedProxy,
        &CloudflareIpRanges::default(),
        &TrustedProxyConfig::default(),
    );

    assert_eq!(
        resolution,
        ClientIpResolution::Accepted("203.0.113.15".parse().unwrap())
    );
}

#[test]
fn trusted_proxy_source_ip_uses_forwarded_header_from_loopback() {
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("Forwarded", "for=203.0.113.15;proto=https")
        .unwrap();

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "127.0.0.1".parse().unwrap(),
        tako_core::SourceIpMode::TrustedProxy,
        &CloudflareIpRanges::default(),
        &TrustedProxyConfig::default(),
    );

    assert_eq!(
        resolution,
        ClientIpResolution::Accepted("203.0.113.15".parse().unwrap())
    );
}

#[test]
fn trusted_proxy_source_ip_uses_configured_trusted_cidr() {
    let config = TrustedProxyConfig::from_raw(false, &["10.0.0.0/8".to_string()], &[]).unwrap();
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("X-Forwarded-For", "203.0.113.15")
        .unwrap();

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "10.1.2.3".parse().unwrap(),
        tako_core::SourceIpMode::TrustedProxy,
        &CloudflareIpRanges::default(),
        &config,
    );

    assert_eq!(
        resolution,
        ClientIpResolution::Accepted("203.0.113.15".parse().unwrap())
    );
}

#[test]
fn strict_trusted_proxy_source_ip_rejects_untrusted_peer() {
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("X-Forwarded-For", "203.0.113.15")
        .unwrap();

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "198.51.100.1".parse().unwrap(),
        tako_core::SourceIpMode::TrustedProxy,
        &CloudflareIpRanges::default(),
        &TrustedProxyConfig::default(),
    );

    assert_eq!(resolution, ClientIpResolution::RejectTrustedProxy);
}

#[test]
fn strict_trusted_proxy_source_ip_rejects_loopback_without_header() {
    let request = RequestHeader::build("GET", b"/", None).expect("build request");

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "127.0.0.1".parse().unwrap(),
        tako_core::SourceIpMode::TrustedProxy,
        &CloudflareIpRanges::default(),
        &TrustedProxyConfig::default(),
    );

    assert_eq!(resolution, ClientIpResolution::RejectTrustedProxy);
}

#[test]
fn direct_source_ip_ignores_cloudflare_header() {
    let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
    request
        .insert_header("CF-Connecting-IP", "203.0.113.15")
        .unwrap();
    let cloudflare_ips = CloudflareIpRanges::from_test_cidrs(&["198.51.100.0/24"]);

    let resolution = client_ip_for_source_ip_mode(
        &request,
        "198.51.100.1".parse().unwrap(),
        tako_core::SourceIpMode::Direct,
        &cloudflare_ips,
        &TrustedProxyConfig::default(),
    );

    assert_eq!(
        resolution,
        ClientIpResolution::Accepted("198.51.100.1".parse().unwrap())
    );
}

#[test]
fn ip_header_value_formats_client_ips() {
    let ipv4 = ip_header_value("198.51.100.1".parse().unwrap());
    let ipv6 = ip_header_value("2001:db8::1".parse().unwrap());

    assert_eq!(ipv4.to_str().ok(), Some("198.51.100.1"));
    assert_eq!(ipv6.to_str().ok(), Some("2001:db8::1"));
}
