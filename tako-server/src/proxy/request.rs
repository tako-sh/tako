use bytes::Bytes;
use pingora_cache::cache_control::CacheControl;
use pingora_cache::filters::{request_cacheable, resp_cacheable};
use pingora_cache::{CacheKey, CacheMetaDefaults, RespCacheable};
use pingora_core::prelude::*;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::Session;
use std::net::IpAddr;
use std::path::Path;
use std::sync::OnceLock;
use tokio::io::AsyncReadExt;

use super::{CloudflareIpRanges, TrustedClientIpHeader, TrustedProxyConfig};

pub(super) fn should_redirect_http_request(
    is_effective_https: bool,
    redirect_http_to_https: bool,
) -> bool {
    redirect_http_to_https && !is_effective_https
}

pub(super) fn https_redirect_host(host: &str, https_port: u16) -> String {
    let host_without_port = strip_host_port(host);
    if https_port == 443 {
        host_without_port.to_string()
    } else {
        format!("{host_without_port}:{https_port}")
    }
}

fn strip_host_port(host: &str) -> &str {
    if let Some(end) = host.find(']')
        && host.starts_with('[')
    {
        return &host[..=end];
    }

    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            name
        }
        _ => host,
    }
}

pub(super) fn is_request_forwarded_https(
    x_forwarded_proto: Option<&str>,
    forwarded: Option<&str>,
) -> bool {
    x_forwarded_proto.is_some_and(x_forwarded_proto_is_https)
        || forwarded.is_some_and(forwarded_header_proto_is_https)
}

#[derive(Clone, Copy)]
pub(super) struct ForwardedHeaderTrust<'a> {
    pub(super) peer_ip: Option<IpAddr>,
    pub(super) cloudflare_ips: &'a CloudflareIpRanges,
    pub(super) trusted_proxy: &'a TrustedProxyConfig,
}

pub(super) fn is_effective_request_https(
    transport_https: bool,
    hostname: &str,
    x_forwarded_for: Option<&str>,
    x_forwarded_proto: Option<&str>,
    forwarded: Option<&str>,
    forwarded_header_trust: ForwardedHeaderTrust<'_>,
) -> bool {
    if transport_https {
        return true;
    }

    if !peer_trusts_forwarded_https(forwarded_header_trust) {
        return false;
    }

    is_request_forwarded_https(x_forwarded_proto, forwarded)
        || should_assume_forwarded_private_request_https(
            hostname,
            x_forwarded_for,
            x_forwarded_proto,
            forwarded,
        )
}

fn peer_trusts_forwarded_https(forwarded_header_trust: ForwardedHeaderTrust<'_>) -> bool {
    let Some(peer_ip) = forwarded_header_trust.peer_ip else {
        return false;
    };

    peer_ip.is_loopback()
        || forwarded_header_trust.cloudflare_ips.contains(&peer_ip)
        || forwarded_header_trust
            .trusted_proxy
            .trusts_proxy_ip(&peer_ip)
}

pub(super) fn should_assume_forwarded_private_request_https(
    hostname: &str,
    x_forwarded_for: Option<&str>,
    x_forwarded_proto: Option<&str>,
    forwarded: Option<&str>,
) -> bool {
    crate::is_private_local_hostname(hostname)
        && has_nonempty_header_value(x_forwarded_for)
        && !has_forwarded_proto(x_forwarded_proto, forwarded)
}

fn has_forwarded_proto(x_forwarded_proto: Option<&str>, forwarded: Option<&str>) -> bool {
    has_nonempty_header_value(x_forwarded_proto)
        || forwarded.is_some_and(forwarded_header_has_proto)
}

fn has_nonempty_header_value(value: Option<&str>) -> bool {
    value.is_some_and(|raw| !raw.trim().is_empty())
}

pub(super) fn x_forwarded_proto_is_https(value: &str) -> bool {
    value
        .split(',')
        .next()
        .map(str::trim)
        .is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
}

pub(super) fn forwarded_header_proto_is_https(value: &str) -> bool {
    value.split(',').any(|entry| {
        entry.split(';').any(|param| {
            let mut parts = param.splitn(2, '=');
            let key = parts.next().map(str::trim).unwrap_or("");
            let raw_value = parts.next().map(str::trim).unwrap_or("");
            let parsed = raw_value.trim_matches('"');
            key.eq_ignore_ascii_case("proto") && parsed.eq_ignore_ascii_case("https")
        })
    })
}

pub(super) fn forwarded_header_has_proto(value: &str) -> bool {
    value.split(',').any(|entry| {
        entry.split(';').any(|param| {
            let mut parts = param.splitn(2, '=');
            let key = parts.next().map(str::trim).unwrap_or("");
            let raw_value = parts.next().map(str::trim).unwrap_or("");
            let parsed = raw_value.trim_matches('"');
            key.eq_ignore_ascii_case("proto") && !parsed.is_empty()
        })
    })
}

pub(super) fn insert_body_headers(
    header: &mut ResponseHeader,
    content_type: &str,
    body: &str,
) -> Result<()> {
    header.insert_header("Content-Type", content_type)?;
    header.insert_header("Content-Length", body.len().to_string())?;
    Ok(())
}

pub(super) fn production_error_body(status: u16) -> &'static str {
    match status {
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Internal Server Error",
    }
}

pub(super) async fn create_production_error_response(
    session: &mut Session,
    status: u16,
) -> Result<bool> {
    let body = production_error_body(status);
    let mut header = ResponseHeader::build(status, None)?;
    insert_body_headers(&mut header, "text/plain", body)?;
    header.insert_header("Cache-Control", "private, no-store")?;
    session
        .as_downstream_mut()
        .write_error_response(header, Bytes::from_static(body.as_bytes()))
        .await?;
    Ok(true)
}

pub(super) fn request_is_proxy_cacheable(request: &RequestHeader) -> bool {
    request_cacheable(request) && !request.headers.contains_key("upgrade")
}

pub(super) fn build_proxy_cache_key(host: &str, uri: &str) -> CacheKey {
    CacheKey::new(
        host.trim().to_ascii_lowercase(),
        uri.as_bytes().to_vec(),
        "",
    )
}

fn response_cache_defaults() -> &'static CacheMetaDefaults {
    static DEFAULTS: OnceLock<CacheMetaDefaults> = OnceLock::new();
    DEFAULTS.get_or_init(|| CacheMetaDefaults::new(|_| None, 0, 0))
}

pub(super) fn response_cacheability(
    resp: &ResponseHeader,
    authorization_present: bool,
) -> RespCacheable {
    let response_for_cache = resp.clone();
    let cache_control = CacheControl::from_resp_headers(&response_for_cache);
    resp_cacheable(
        cache_control.as_ref(),
        response_for_cache,
        authorization_present,
        response_cache_defaults(),
    )
}

pub(super) async fn stream_static_file(
    session: &mut Session,
    file: &mut tokio::fs::File,
    path: &Path,
) -> Result<()> {
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut buffer = vec![0_u8; CHUNK_SIZE];

    loop {
        let bytes_read = file.read(&mut buffer).await.map_err(|e| {
            Error::explain(
                ErrorType::InternalError,
                format!("Failed to read static asset {}: {}", path.display(), e),
            )
        })?;

        if bytes_read == 0 {
            session.write_response_body(None, true).await?;
            break;
        }

        session
            .write_response_body(Some(buffer[..bytes_read].to_vec().into()), false)
            .await?;
    }

    Ok(())
}

pub(super) fn client_ip_from_session(session: &Session) -> Option<IpAddr> {
    session
        .digest()
        .and_then(|d| d.socket_digest.as_ref())
        .and_then(|sd| sd.peer_addr())
        .and_then(|addr| addr.as_inet())
        .map(|inet| inet.ip())
}

pub(super) fn client_ip_from_trusted_headers(
    request: &RequestHeader,
    peer_ip: IpAddr,
    trusted_proxy: &TrustedProxyConfig,
) -> Option<IpAddr> {
    if trusted_proxy.client_ip_headers.is_empty() || !trusted_proxy.trusts_proxy_ip(&peer_ip) {
        return None;
    }

    trusted_proxy
        .client_ip_headers
        .iter()
        .find_map(|header| client_ip_from_header(request, *header))
}

pub(super) fn client_ip_from_trusted_proxy_source_headers(
    request: &RequestHeader,
    peer_ip: IpAddr,
    trusted_proxy: &TrustedProxyConfig,
) -> Option<IpAddr> {
    if !peer_ip.is_loopback() && !trusted_proxy.trusts_proxy_ip(&peer_ip) {
        return None;
    }

    if trusted_proxy.client_ip_headers.is_empty() {
        return [
            TrustedClientIpHeader::XForwardedFor,
            TrustedClientIpHeader::Forwarded,
        ]
        .into_iter()
        .find_map(|header| client_ip_from_header(request, header));
    }

    trusted_proxy
        .client_ip_headers
        .iter()
        .find_map(|header| client_ip_from_header(request, *header))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientIpResolution {
    Accepted(IpAddr),
    RejectCloudflareProxy,
    RejectTrustedProxy,
}

pub(super) fn client_ip_for_source_ip_mode(
    request: &RequestHeader,
    peer_ip: IpAddr,
    mode: tako_core::SourceIpMode,
    cloudflare_ips: &CloudflareIpRanges,
    trusted_proxy: &TrustedProxyConfig,
) -> ClientIpResolution {
    match mode {
        tako_core::SourceIpMode::Auto => {
            let ip = client_ip_from_cloudflare_proxy(request, peer_ip, cloudflare_ips)
                .or_else(|| client_ip_from_trusted_headers(request, peer_ip, trusted_proxy))
                .unwrap_or(peer_ip);
            ClientIpResolution::Accepted(ip)
        }
        tako_core::SourceIpMode::Direct => ClientIpResolution::Accepted(peer_ip),
        tako_core::SourceIpMode::CloudflareProxy => {
            match client_ip_from_cloudflare_proxy(request, peer_ip, cloudflare_ips) {
                Some(ip) => ClientIpResolution::Accepted(ip),
                None => ClientIpResolution::RejectCloudflareProxy,
            }
        }
        tako_core::SourceIpMode::TrustedProxy => {
            match client_ip_from_trusted_proxy_source_headers(request, peer_ip, trusted_proxy) {
                Some(ip) => ClientIpResolution::Accepted(ip),
                None => ClientIpResolution::RejectTrustedProxy,
            }
        }
    }
}

fn client_ip_from_cloudflare_proxy(
    request: &RequestHeader,
    peer_ip: IpAddr,
    cloudflare_ips: &CloudflareIpRanges,
) -> Option<IpAddr> {
    if !cloudflare_ips.contains(&peer_ip) {
        return None;
    }

    client_ip_from_header(request, TrustedClientIpHeader::CfConnectingIp)
}

fn client_ip_from_header(request: &RequestHeader, header: TrustedClientIpHeader) -> Option<IpAddr> {
    let value = request
        .headers
        .get(header.as_str())
        .and_then(|value| value.to_str().ok())?;

    match header {
        TrustedClientIpHeader::CfConnectingIp => parse_header_ip(value),
        TrustedClientIpHeader::Forwarded => parse_forwarded_for(value),
        TrustedClientIpHeader::XForwardedFor => value.split(',').next().and_then(parse_header_ip),
    }
}

fn parse_header_ip(value: &str) -> Option<IpAddr> {
    value.trim().parse().ok()
}

fn parse_forwarded_for(value: &str) -> Option<IpAddr> {
    let first_entry = value.split(',').next()?;
    let raw_for = first_entry.split(';').find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("for")
            .then(|| value.trim().trim_matches('"'))
    })?;

    parse_forwarded_for_value(raw_for)
}

fn parse_forwarded_for_value(value: &str) -> Option<IpAddr> {
    if let Some(rest) = value.strip_prefix('[') {
        let (ip, _) = rest.split_once(']')?;
        return ip.parse().ok();
    }

    if let Ok(ip) = value.parse() {
        return Some(ip);
    }

    let (host, port) = value.rsplit_once(':')?;
    if host.contains(':') || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    host.parse().ok()
}

pub(super) fn request_host(req: &pingora_http::RequestHeader) -> &str {
    req.uri
        .authority()
        .map(|a| a.as_str())
        .or_else(|| req.headers.get("host").and_then(|h| h.to_str().ok()))
        .unwrap_or("")
}

pub(super) fn path_looks_like_static_asset(path: &str) -> bool {
    let final_segment = path.rsplit_once('/').map_or(path, |(_, segment)| segment);
    final_segment.contains('.') && !final_segment.ends_with('.')
}

pub(super) fn path_uses_tako_handler(path: &str) -> bool {
    path_looks_like_static_asset(path)
        || path == tako_images::PUBLIC_IMAGE_BASE_PATH
        || path.starts_with(tako_channels::CHANNELS_BASE_PATH)
}

pub(super) fn static_lookup_paths(
    request_path: &str,
    matched_route_path: Option<&str>,
) -> Vec<String> {
    let mut candidates = vec![request_path.to_string()];
    if let Some(route_path) = matched_route_path
        && let Some(stripped) = strip_route_prefix_for_static_lookup(request_path, route_path)
        && stripped != request_path
    {
        candidates.push(stripped);
    }
    candidates
}

pub(super) fn strip_route_prefix_for_static_lookup(
    request_path: &str,
    route_path: &str,
) -> Option<String> {
    let prefix = if let Some(p) = route_path.strip_suffix("/*") {
        p
    } else if let Some(p) = route_path.strip_suffix('*') {
        p
    } else {
        route_path
    };

    if request_path == prefix {
        return Some("/".to_string());
    }

    let stripped = request_path.strip_prefix(prefix)?;
    if stripped.is_empty() {
        return Some("/".to_string());
    }
    if stripped.starts_with('/') {
        Some(stripped.to_string())
    } else {
        Some(format!("/{}", stripped))
    }
}
