use crate::{IMAGE_BASE_PATH, ImageError, ImagesConfig, PUBLIC_IMAGE_BASE_PATH};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

const MAX_SOURCE_CHARS: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    LocalPath(String),
    RemoteUrl(String),
}

impl ImageSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::LocalPath(path) | Self::RemoteUrl(path) => path,
        }
    }
}

pub(crate) fn parse_source(source: &str) -> Result<ImageSource, ImageError> {
    if source.is_empty()
        || source.len() > MAX_SOURCE_CHARS
        || source.bytes().any(|byte| byte.is_ascii_control())
        || source.trim() != source
        || source.contains('#')
        || path_has_dot_segments(source)
    {
        return Err(ImageError::InvalidSource);
    }

    if source.starts_with('/') {
        if source.starts_with("//")
            || source.starts_with(IMAGE_BASE_PATH)
            || source.starts_with(PUBLIC_IMAGE_BASE_PATH)
        {
            return Err(ImageError::InvalidSource);
        }
        let url = Url::parse(&format!("https://tako.invalid{source}"))
            .map_err(|_| ImageError::InvalidSource)?;
        return Ok(ImageSource::LocalPath(
            url[url::Position::BeforePath..].to_string(),
        ));
    }

    let url = Url::parse(source).map_err(|_| ImageError::InvalidSource)?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(ImageError::InvalidSource),
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(ImageError::InvalidSource);
    }
    let Some(host) = url.host() else {
        return Err(ImageError::InvalidSource);
    };
    if host_is_private_or_local(host) {
        return Err(ImageError::InvalidSource);
    }

    Ok(ImageSource::RemoteUrl(url.to_string()))
}

pub(crate) fn validate_public_source_allowed(
    source: &ImageSource,
    config: &ImagesConfig,
) -> Result<(), ImageError> {
    match source {
        ImageSource::LocalPath(path) => {
            let default_patterns = ["/**".to_string()];
            let patterns = config
                .local_patterns
                .as_deref()
                .unwrap_or(&default_patterns);
            if patterns.iter().any(|pattern| {
                path_pattern_matches(pattern, path.split('?').next().unwrap_or(path))
            }) {
                Ok(())
            } else {
                Err(ImageError::InvalidSignature)
            }
        }
        ImageSource::RemoteUrl(url) => {
            if config
                .remote_patterns
                .iter()
                .any(|pattern| remote_pattern_matches(pattern, url))
            {
                Ok(())
            } else {
                Err(ImageError::InvalidSignature)
            }
        }
    }
}

pub(crate) fn validate_pattern_list(patterns: &[String], local: bool) -> Result<(), ImageError> {
    for pattern in patterns {
        if pattern.trim() != pattern || pattern.is_empty() {
            return Err(ImageError::InvalidSource);
        }
        if local {
            if !pattern.starts_with('/') || pattern.starts_with("//") || pattern.contains('?') {
                return Err(ImageError::InvalidSource);
            }
        } else {
            validate_remote_pattern(pattern)?;
        }
    }
    Ok(())
}

fn validate_remote_pattern(pattern: &str) -> Result<(), ImageError> {
    let normalized = normalize_remote_pattern(pattern);
    let parsed = Url::parse(&normalized).map_err(|_| ImageError::InvalidSource)?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ImageError::InvalidSource);
    }
    if parsed.username() != "" || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(ImageError::InvalidSource);
    }
    let host = parsed.host_str().ok_or(ImageError::InvalidSource)?;
    if host.is_empty() || host.contains('*') && !host.starts_with("*.") {
        return Err(ImageError::InvalidSource);
    }
    Ok(())
}

fn normalize_remote_pattern(pattern: &str) -> String {
    if pattern.contains("://") {
        pattern.to_string()
    } else {
        format!("https://{pattern}")
    }
}

fn remote_pattern_matches(pattern: &str, source: &str) -> bool {
    let has_explicit_scheme = pattern.contains("://");
    let Ok(pattern_url) = Url::parse(&normalize_remote_pattern(pattern)) else {
        return false;
    };
    let Ok(source_url) = Url::parse(source) else {
        return false;
    };
    if has_explicit_scheme && pattern_url.scheme() != source_url.scheme() {
        return false;
    }
    if !matches!(source_url.scheme(), "http" | "https") {
        return false;
    }
    let authority = pattern
        .split_once("://")
        .map_or(pattern, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let explicit_port = authority.rsplit_once(':').is_some_and(|(_, port)| {
        !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
    });
    if has_explicit_scheme || explicit_port {
        if pattern_url.port_or_known_default() != source_url.port_or_known_default() {
            return false;
        }
    } else if source_url.port().is_some() {
        // An omitted scheme allows HTTP or HTTPS on their standard ports.
        return false;
    }
    let Some(pattern_host) = pattern_url.host_str() else {
        return false;
    };
    let Some(source_host) = source_url.host_str() else {
        return false;
    };
    if !host_pattern_matches(pattern_host, source_host) {
        return false;
    }
    path_pattern_matches(pattern_url.path(), source_url.path())
}

fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host.ends_with(&format!(".{suffix}")) && host != suffix;
    }
    pattern == host
}

fn path_has_dot_segments(path: &str) -> bool {
    // Reject ambiguous separators and dot segments before URL parsing can
    // normalize them outside the allowed path. Check nested encodings too,
    // since an upstream app may decode a path again.
    let mut path = path.split('?').next().unwrap_or(path).to_ascii_lowercase();
    loop {
        if path.contains('\\') || path.contains("%2f") || path.contains("%5c") {
            return true;
        }
        let decoded = path.replace("%2e", ".").replace("%25", "%");
        if decoded
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return true;
        }
        if decoded == path {
            return false;
        }
        path = decoded;
    }
}

fn path_pattern_matches(pattern: &str, path: &str) -> bool {
    if pattern == "/**" {
        return path.starts_with('/');
    }
    let pattern_parts = split_path_segments(pattern);
    let path_parts = split_path_segments(path);
    path_segments_match(&pattern_parts, &path_parts)
}

fn split_path_segments(path: &str) -> Vec<&str> {
    path.trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn path_segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((segment, [])) if *segment == "**" => true,
        Some((segment, rest)) if *segment == "**" => {
            (0..=path.len()).any(|index| path_segments_match(rest, &path[index..]))
        }
        Some((segment, rest)) if *segment == "*" => {
            !path.is_empty() && path_segments_match(rest, &path[1..])
        }
        Some((segment, rest)) => {
            !path.is_empty() && *segment == path[0] && path_segments_match(rest, &path[1..])
        }
    }
}

fn host_is_private_or_local(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain_is_private_or_local(domain),
        Host::Ipv4(ip) => ipv4_is_private_or_local(ip),
        Host::Ipv6(ip) => ipv6_is_private_or_local(ip),
    }
}

fn domain_is_private_or_local(domain: &str) -> bool {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    domain.is_empty()
        || !domain.contains('.')
        || domain == "localhost"
        || domain.ends_with(".localhost")
        || domain == "local"
        || domain.ends_with(".local")
        || domain.parse::<IpAddr>().is_ok_and(ip_is_private_or_local)
}

/// Whether a remote image fetch must refuse `ip`. Only ordinary public unicast
/// addresses pass; private, local, reserved, documentation, and benchmarking
/// ranges do not, nor does an IPv6 form that leads to such an IPv4 address.
pub fn ip_is_private_or_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ipv4_is_private_or_local(ip),
        IpAddr::V6(ip) => ipv6_is_private_or_local(ip),
    }
}

fn ipv4_is_private_or_local(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        // 192.0.2.0/24, 198.51.100.0/24, and 203.0.113.0/24 (RFC 5737).
        || ip.is_documentation()
        // 0.0.0.0/8 "this network" (RFC 791), including the unspecified address.
        || a == 0
        // 100.64.0.0/10 (RFC 6598 shared address space) — used by CGNAT and
        // overlay networks like Tailscale; reachable hosts there are private.
        || (a == 100 && (b & 0xc0) == 64)
        // 192.0.0.0/24 IETF protocol assignments (RFC 6890).
        || (a == 192 && b == 0 && c == 0)
        // 192.88.99.0/24 deprecated 6to4 relay anycast (RFC 7526).
        || (a == 192 && b == 88 && c == 99)
        // 198.18.0.0/15 benchmarking (RFC 2544).
        || (a == 198 && (b & 0xfe) == 18)
        // 240.0.0.0/4 reserved (RFC 1112), including limited broadcast.
        || a >= 240
}

fn ipv6_is_private_or_local(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    // IPv4-mapped (::ffff:0:0/96) and well-known NAT64 (64:ff9b::/96, RFC 6052)
    // addresses reach the IPv4 host in their last 32 bits, so that host decides.
    if let [0, 0, 0, 0, 0, 0xffff, high, low] | [0x64, 0xff9b, 0, 0, 0, 0, high, low] = segments {
        let embedded = Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
        return ipv4_is_private_or_local(embedded);
    }
    // Global unicast is allocated from 2000::/3 only (RFC 4291). Everything
    // else is loopback, unspecified, IPv4-compatible (::/96), local-use NAT64
    // (64:ff9b:1::/48), discard-only (100::/64), unique-local (fc00::/7),
    // link-local (fe80::/10), site-local, multicast (ff00::/8), or unallocated.
    if segments[0] & 0xe000 != 0x2000 {
        return true;
    }
    match segments {
        // 2001::/23 IETF protocol assignments, including Teredo (2001::/32),
        // whose embedded IPv4 addresses are not checked individually.
        [0x2001, second, ..] if second < 0x200 => true,
        // 2001:db8::/32 documentation (RFC 3849).
        [0x2001, 0xdb8, ..] => true,
        // 2002::/16 6to4 (RFC 3056) tunnels to its embedded IPv4 address and
        // is deprecated for public use, so the whole block is refused.
        [0x2002, ..] => true,
        // 3fff::/20 documentation (RFC 9637).
        [0x3fff, second, ..] if second < 0x1000 => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_every_non_public_ipv4_class() {
        for (class, addresses) in [
            (
                "this network 0.0.0.0/8",
                &["0.0.0.0", "0.1.2.3", "0.255.255.255"][..],
            ),
            ("private 10.0.0.0/8", &["10.0.0.1", "10.255.255.255"]),
            (
                "shared 100.64.0.0/10",
                &["100.64.0.1", "100.100.100.100", "100.127.255.255"],
            ),
            ("loopback 127.0.0.0/8", &["127.0.0.1", "127.255.255.254"]),
            (
                "link-local 169.254.0.0/16",
                &["169.254.0.1", "169.254.169.254"],
            ),
            ("private 172.16.0.0/12", &["172.16.0.1", "172.31.255.255"]),
            (
                "protocol assignments 192.0.0.0/24",
                &["192.0.0.0", "192.0.0.8", "192.0.0.170", "192.0.0.255"],
            ),
            ("documentation 192.0.2.0/24", &["192.0.2.0", "192.0.2.255"]),
            (
                "6to4 relay anycast 192.88.99.0/24",
                &["192.88.99.0", "192.88.99.255"],
            ),
            (
                "private 192.168.0.0/16",
                &["192.168.0.1", "192.168.255.255"],
            ),
            (
                "benchmarking 198.18.0.0/15",
                &["198.18.0.0", "198.19.255.255"],
            ),
            (
                "documentation 198.51.100.0/24",
                &["198.51.100.0", "198.51.100.255"],
            ),
            (
                "documentation 203.0.113.0/24",
                &["203.0.113.0", "203.0.113.255"],
            ),
            ("multicast 224.0.0.0/4", &["224.0.0.1", "239.255.255.255"]),
            (
                "reserved 240.0.0.0/4",
                &["240.0.0.0", "250.1.2.3", "255.255.255.254"],
            ),
            ("broadcast", &["255.255.255.255"]),
        ] {
            for ip in addresses {
                let ip: IpAddr = ip.parse().unwrap();
                assert!(
                    ip_is_private_or_local(ip),
                    "{ip} ({class}) must be rejected"
                );
            }
        }
    }

    #[test]
    fn rejects_every_non_public_ipv6_class() {
        for (class, addresses) in [
            ("unspecified", &["::"][..]),
            ("loopback", &["::1"]),
            (
                "IPv4-compatible ::/96",
                &["::10.0.0.1", "::127.0.0.1", "::8.8.8.8", "::ffff"],
            ),
            (
                "IPv4-mapped ::ffff:0:0/96 to a non-public IPv4",
                &[
                    "::ffff:10.0.0.1",
                    "::ffff:127.0.0.1",
                    "::ffff:169.254.169.254",
                    "::ffff:192.0.0.8",
                    "::ffff:198.18.0.1",
                    "::ffff:203.0.113.9",
                    "::ffff:240.0.0.1",
                    "::ffff:255.255.255.255",
                ],
            ),
            (
                "IPv4-translated ::ffff:0:0:0/96",
                &["::ffff:0:10.0.0.1", "::ffff:0:8.8.8.8"],
            ),
            (
                "NAT64 64:ff9b::/96 to a non-public IPv4",
                &[
                    "64:ff9b::10.0.0.1",
                    "64:ff9b::127.0.0.1",
                    "64:ff9b::169.254.169.254",
                    "64:ff9b::192.168.1.1",
                    "64:ff9b::100.64.0.1",
                    "64:ff9b::198.19.0.1",
                    "64:ff9b::255.255.255.255",
                ],
            ),
            (
                "local-use NAT64 64:ff9b:1::/48",
                &[
                    "64:ff9b:1::10.0.0.1",
                    "64:ff9b:1::8.8.8.8",
                    "64:ff9b:1:ffff::1",
                ],
            ),
            (
                "discard 100::/64",
                &["100::", "100::1", "100::ffff:ffff:ffff:ffff"],
            ),
            (
                "Teredo 2001::/32",
                &["2001::1", "2001:0:4136:e378:8000:63bf:3fff:fdd2"],
            ),
            (
                "protocol assignments 2001::/23",
                &["2001:1::1", "2001:2::1", "2001:20::1", "2001:1ff::1"],
            ),
            (
                "documentation 2001:db8::/32",
                &["2001:db8::", "2001:db8::1", "2001:db8:ffff::1"],
            ),
            (
                "6to4 2002::/16",
                &[
                    "2002::1",
                    "2002:a00:1::1",
                    "2002:7f00:1::1",
                    "2002:808:808::1",
                ],
            ),
            ("documentation 3fff::/20", &["3fff::1", "3fff:fff:ffff::1"]),
            ("segment routing 5f00::/16", &["5f00::1"]),
            (
                "unique-local fc00::/7",
                &["fc00::1", "fd00:ec2::254", "fdff:ffff::1"],
            ),
            ("link-local fe80::/10", &["fe80::1", "febf::1"]),
            ("site-local fec0::/10", &["fec0::1", "feff::1"]),
            ("multicast ff00::/8", &["ff02::1", "ff0e::1"]),
            (
                "unallocated outside 2000::/3",
                &["4000::1", "8000::1", "e000::1", "1::1"],
            ),
        ] {
            for ip in addresses {
                let ip: IpAddr = ip.parse().unwrap();
                assert!(
                    ip_is_private_or_local(ip),
                    "{ip} ({class}) must be rejected"
                );
            }
        }
    }

    #[test]
    fn allows_public_ipv6_addresses() {
        for ip in [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "2a00:1450:4001:81b::200e",
            "2001:200::1",
            "2001:db7:ffff::1",
            "2001:db9::1",
            "2003::1",
            "2400:cb00::1",
            "3fff:1000::1",
            // The embedded IPv4 address is the destination and it is public.
            "::ffff:1.1.1.1",
            "::ffff:8.8.8.8",
            "64:ff9b::1.1.1.1",
            "64:ff9b::8.8.8.8",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!ip_is_private_or_local(ip), "{ip} should be public");
        }
    }

    #[test]
    fn remote_sources_reject_non_public_address_literals() {
        for host in [
            "192.0.0.8",
            "198.18.0.1",
            "203.0.113.7",
            "240.0.0.1",
            "255.255.255.255",
            "0.0.0.1",
            // Legacy spellings the URL parser normalizes to IPv4.
            "0x7f.1",
            "2130706433",
            "[::ffff:10.0.0.1]",
            "[::ffff:a00:1]",
            "[::10.0.0.1]",
            "[64:ff9b::a00:1]",
            "[64:ff9b:1::a00:1]",
            "[100::1]",
            "[2001:db8::1]",
            "[2002:a00:1::1]",
            "[2001::1]",
            "[fd00::1]",
            "[fe80::1]",
            "[ff02::1]",
        ] {
            for url in [
                format!("http://{host}/a.png"),
                format!("https://{host}:8443/a.png"),
            ] {
                assert_eq!(parse_source(&url), Err(ImageError::InvalidSource), "{url}");
            }
        }
        for host in ["1.1.1.1", "[2606:4700:4700::1111]", "[64:ff9b::808:808]"] {
            let url = format!("https://{host}/a.png");
            assert_eq!(
                parse_source(&url),
                Ok(ImageSource::RemoteUrl(url.clone())),
                "{url}"
            );
        }
    }

    #[test]
    fn parse_source_rejects_dot_segments() {
        assert_eq!(
            parse_source("/images/../../secret.png"),
            Err(ImageError::InvalidSource)
        );
        assert_eq!(
            parse_source("/images/./avatar.png"),
            Err(ImageError::InvalidSource)
        );
        assert_eq!(
            parse_source("https://cdn.example.com/uploads/../admin/secret.png"),
            Err(ImageError::InvalidSource)
        );
    }

    #[test]
    fn rejects_paths_that_change_scope_when_normalized() {
        for path in [
            "/images/%2e%2e/private.png",
            "/images/.%2E/private.png",
            "/images/%252e%252e/private.png",
            "/images/..\\private.png",
            "/images/%2f../private.png",
            "/images/%5c../private.png",
            "/images/.\t./private.png",
            "/images/..?ignored=/avatar.png",
        ] {
            assert_eq!(parse_source(path), Err(ImageError::InvalidSource), "{path}");
            let remote = format!("https://cdn.example.com{path}");
            assert_eq!(
                parse_source(&remote),
                Err(ImageError::InvalidSource),
                "{remote}"
            );
        }
    }

    #[test]
    fn remote_patterns_constrain_ports() {
        assert!(!remote_pattern_matches(
            "https://cdn.example.com:8443/**",
            "https://cdn.example.com:9443/a.png"
        ));
        assert!(!remote_pattern_matches(
            "https://cdn.example.com/**",
            "https://cdn.example.com:8443/a.png"
        ));
        assert!(!remote_pattern_matches(
            "cdn.example.com:443/**",
            "http://cdn.example.com/a.png"
        ));
        assert!(remote_pattern_matches(
            "cdn.example.com/**",
            "http://cdn.example.com/a.png"
        ));
        assert!(remote_pattern_matches(
            "cdn.example.com/**",
            "https://cdn.example.com/a.png"
        ));
        assert!(remote_pattern_matches(
            "cdn.example.com:8443/**",
            "http://cdn.example.com:8443/a.png"
        ));
    }

    #[test]
    fn local_pattern_checks_path_without_query() {
        let config = ImagesConfig {
            local_patterns: Some(vec!["/images/avatar.png".into()]),
            ..Default::default()
        };
        let source = parse_source("/images/avatar.png?version=2").unwrap();
        validate_public_source_allowed(&source, &config).unwrap();
    }

    #[test]
    fn allows_public_ipv4_addresses() {
        for ip in [
            "1.0.0.0",
            "1.1.1.1",
            "8.8.8.8",
            "9.255.255.255",
            "11.0.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "126.255.255.255",
            "128.0.0.1",
            "169.253.255.255",
            "169.255.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "191.255.255.255",
            "192.0.1.255",
            "192.0.3.0",
            "192.88.98.255",
            "192.88.100.0",
            "192.167.255.255",
            "192.169.0.1",
            "198.17.255.255",
            "198.20.0.0",
            "198.51.99.255",
            "198.51.101.0",
            "203.0.112.255",
            "203.0.114.0",
            "223.255.255.255",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!ip_is_private_or_local(ip), "{ip} should be public");
        }
    }
}
