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

pub fn ip_is_private_or_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ipv4_is_private_or_local(ip),
        IpAddr::V6(ip) => ipv6_is_private_or_local(ip),
    }
}

fn ipv4_is_private_or_local(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        // 100.64.0.0/10 (RFC 6598 shared address space) — used by CGNAT and
        // overlay networks like Tailscale; reachable hosts there are private.
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
}

fn ipv6_is_private_or_local(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return ipv4_is_private_or_local(mapped);
    }

    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ((ip.segments()[0] & 0xfe00) == 0xfc00)
        || ((ip.segments()[0] & 0xffc0) == 0xfe80)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_private_and_shared_ipv4_ranges() {
        for ip in [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.100.100.100",
            "100.127.255.255",
            "0.0.0.0",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(ip_is_private_or_local(ip), "{ip} should be private/local");
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
        for ip in ["1.1.1.1", "8.8.8.8", "100.63.255.255", "100.128.0.1"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!ip_is_private_or_local(ip), "{ip} should be public");
        }
    }
}
