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
        || source.contains('\0')
        || source.contains('\r')
        || source.contains('\n')
        || source.contains('#')
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
        return Ok(ImageSource::LocalPath(source.to_string()));
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

    Ok(ImageSource::RemoteUrl(source.to_string()))
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
            if patterns
                .iter()
                .any(|pattern| path_pattern_matches(pattern, path))
            {
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
}

fn ipv6_is_private_or_local(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ((ip.segments()[0] & 0xfe00) == 0xfc00)
        || ((ip.segments()[0] & 0xffc0) == 0xfe80)
}
