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
fn forwarded_chains_ignore_entries_the_client_supplied() {
    let config = TrustedProxyConfig::from_raw(
        false,
        &["10.0.0.0/8".to_string()],
        &["x-forwarded-for".to_string(), "forwarded".to_string()],
    )
    .unwrap();
    let peer = "10.0.0.2".parse().unwrap();
    let resolve = |name: &str, values: &[&str]| {
        let mut request = RequestHeader::build("GET", b"/", None).expect("build request");
        for value in values {
            request.append_header(name.to_string(), *value).unwrap();
        }
        client_ip_from_trusted_headers(&request, peer, &config)
    };
    let client = Some("203.0.113.15".parse().unwrap());
    // The trusted proxy appended the address it saw after the client's value.
    assert_eq!(
        resolve("X-Forwarded-For", &["198.51.100.66, 203.0.113.15"]),
        client
    );
    assert_eq!(
        resolve("X-Forwarded-For", &["not-an-ip, 203.0.113.15"]),
        client
    );
    // Trusted hops are skipped, across repeated header lines.
    assert_eq!(
        resolve(
            "X-Forwarded-For",
            &["198.51.100.66", "203.0.113.15, 10.1.1.1"]
        ),
        client
    );
    assert_eq!(
        resolve(
            "Forwarded",
            &["for=198.51.100.66, for=203.0.113.15;proto=https, for=\"10.1.1.1\""]
        ),
        client
    );
    // A quoted comma or semicolon does not end an element or parameter.
    assert_eq!(
        resolve(
            "Forwarded",
            &[r#"for=203.0.113.15;by="edge, zone", for=10.1.1.1;by="a;for=10.2.2.2""#]
        ),
        client
    );
    // A malformed hop cannot be skipped to reach a client-supplied address.
    assert_eq!(
        resolve("X-Forwarded-For", &["198.51.100.66, garbage"]),
        None
    );
    // A request that only crossed trusted hops keeps the original address.
    assert_eq!(
        resolve("X-Forwarded-For", &["10.9.9.9, 10.1.1.1"]),
        "10.9.9.9".parse().ok()
    );
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
