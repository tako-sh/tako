use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use reqwest::{Client, ClientBuilder, Url, redirect::Policy};
use sha2::{Digest, Sha256};
use tako_images::{
    IMAGE_BASE_PATH, ImageError, ImageSource, PUBLIC_IMAGE_BASE_PATH, TransformLimits,
    TransformOptions, cache_control, ip_is_private_or_local, transform_image, verify_image_path,
    verify_public_image_request,
};
use tokio::net::lookup_host;
use tokio::time::timeout;

use crate::proxy::RouteTarget;

const IMAGE_ERROR_CACHE_CONTROL: &str = "private, no-store";

pub(crate) fn is_image_request_path(path: &str) -> bool {
    path == PUBLIC_IMAGE_BASE_PATH
        || path
            .strip_prefix(IMAGE_BASE_PATH)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(crate) async fn try_handle(
    session: &mut Session,
    target: &RouteTarget,
    path: &str,
    host: &str,
    method: &str,
) -> Result<bool> {
    if !is_image_request_path(path) {
        return Ok(false);
    }
    if method != "GET" && method != "HEAD" {
        return write_image_error(session, 405, "Method Not Allowed").await;
    }

    let accept = session
        .req_header()
        .headers
        .get("accept")
        .and_then(|value| value.to_str().ok());
    let verified =
        match verify_image_request(path, session.req_header().uri.query(), accept, target) {
            Ok(verified) => verified,
            Err(error) => {
                let status = image_error_status(&error);
                return write_image_error(session, status, image_error_body(status)).await;
            }
        };

    let limits = TransformLimits::default();
    let source =
        match load_image_source(&verified.source, target.upstream_port, host, &limits).await {
            Ok(source) => source,
            Err(error) => {
                let status = image_error_status(&error);
                return write_image_error(session, status, image_error_body(status)).await;
            }
        };

    let transformed = match transform_image_blocking(
        source,
        TransformOptions {
            format: verified.format,
            width: verified.width,
            height: verified.height,
            fit: verified.fit,
            crop: verified.crop,
            quality: verified.quality,
        },
        limits,
    )
    .await
    {
        Ok(transformed) => transformed,
        Err(error) => {
            let status = image_error_status(&error);
            return write_image_error(session, status, image_error_body(status)).await;
        }
    };

    let mut header = ResponseHeader::build(200, None)?;
    header.insert_header("Content-Type", transformed.content_type)?;
    header.insert_header("Content-Length", transformed.bytes.len().to_string())?;
    let cache_control_header =
        cache_control(verified.visibility, verified.private_browser_cache_max_age);
    header.insert_header("Cache-Control", cache_control_header.as_ref())?;
    header.insert_header("ETag", image_etag(path, transformed.content_type))?;
    session
        .write_response_header(Box::new(header), false)
        .await?;

    if method == "HEAD" {
        session.write_response_body(None, true).await?;
    } else {
        session
            .write_response_body(Some(Bytes::from(transformed.bytes)), true)
            .await?;
    }

    Ok(true)
}

fn verify_image_request(
    path: &str,
    query: Option<&str>,
    accept: Option<&str>,
    target: &RouteTarget,
) -> Result<tako_images::VerifiedImageRequest, ImageError> {
    if path == PUBLIC_IMAGE_BASE_PATH {
        return verify_public_image_request(path, query, accept, &target.images);
    }
    if query.is_some() {
        return Err(ImageError::InvalidUrl);
    }
    if target.image_secret.is_empty() {
        return Err(ImageError::InvalidSignature);
    }
    verify_image_path(&target.image_secret, path, unix_now_secs())
}

struct ImageSourceBytes {
    bytes: Vec<u8>,
    content_type: Option<String>,
}

async fn load_image_source(
    source: &ImageSource,
    upstream_port: u16,
    host: &str,
    limits: &TransformLimits,
) -> Result<ImageSourceBytes, ImageError> {
    match source {
        ImageSource::LocalPath(path) => {
            fetch_image_source(
                image_http_client(),
                &format!("http://127.0.0.1:{upstream_port}{path}"),
                Some(host),
                limits,
            )
            .await
        }
        ImageSource::RemoteUrl(url) => fetch_remote_image_source(url, limits).await,
    }
}

async fn transform_image_blocking(
    source: ImageSourceBytes,
    options: TransformOptions,
    limits: TransformLimits,
) -> Result<tako_images::TransformedImage, ImageError> {
    tokio::task::spawn_blocking(move || {
        transform_image(
            &source.bytes,
            source.content_type.as_deref(),
            options,
            &limits,
        )
    })
    .await
    .map_err(|_| ImageError::TransformFailed)?
}

async fn fetch_remote_image_source(
    url: &str,
    limits: &TransformLimits,
) -> Result<ImageSourceBytes, ImageError> {
    let target = RemoteFetchTarget::resolve(url).await?;
    let guarded_client;
    let client = match target {
        RemoteFetchTarget::IpLiteral => image_http_client(),
        RemoteFetchTarget::Resolved { host, addrs } => {
            guarded_client = guarded_image_http_client(&host, &addrs)?;
            &guarded_client
        }
    };
    fetch_image_source(client, url, None, limits).await
}

async fn fetch_image_source(
    client: &Client,
    url: &str,
    host_header: Option<&str>,
    limits: &TransformLimits,
) -> Result<ImageSourceBytes, ImageError> {
    let mut request = client.get(url);
    if let Some(host) = host_header {
        request = request.header("Host", host);
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    if !response.status().is_success() {
        return Err(ImageError::InvalidSource);
    }
    if response
        .content_length()
        .is_some_and(|len| len > limits.max_source_bytes as u64)
    {
        return Err(ImageError::SourceTooLarge);
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = read_response_body_limited(&mut response, limits.max_source_bytes).await?;
    Ok(ImageSourceBytes {
        bytes,
        content_type,
    })
}

enum RemoteFetchTarget {
    IpLiteral,
    Resolved {
        host: String,
        addrs: Vec<SocketAddr>,
    },
}

impl RemoteFetchTarget {
    async fn resolve(url: &str) -> Result<Self, ImageError> {
        let parsed = Url::parse(url).map_err(|_| ImageError::InvalidSource)?;
        let host = parsed.host_str().ok_or(ImageError::InvalidSource)?;
        if host.parse::<IpAddr>().is_ok() {
            return Ok(Self::IpLiteral);
        }

        let port = parsed
            .port_or_known_default()
            .ok_or(ImageError::InvalidSource)?;
        let addrs = resolve_remote_addrs(host, port).await?;
        Ok(Self::Resolved {
            host: host.to_string(),
            addrs,
        })
    }
}

async fn resolve_remote_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, ImageError> {
    let addrs = timeout(Duration::from_secs(3), lookup_host((host, port)))
        .await
        .map_err(|_| ImageError::TransformFailed)?
        .map_err(|_| ImageError::InvalidSource)?
        .collect::<Vec<_>>();
    validate_remote_resolved_addrs(&addrs)?;
    Ok(addrs)
}

fn validate_remote_resolved_addrs(addrs: &[SocketAddr]) -> Result<(), ImageError> {
    if addrs.is_empty() || addrs.iter().any(|addr| ip_is_private_or_local(addr.ip())) {
        return Err(ImageError::InvalidSource);
    }
    Ok(())
}

async fn read_response_body_limited(
    response: &mut reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ImageError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ImageError::TransformFailed)?
    {
        append_limited_body_chunk(&mut bytes, &chunk, max_bytes)?;
    }
    Ok(bytes)
}

fn append_limited_body_chunk(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: usize,
) -> Result<(), ImageError> {
    let next_len = bytes
        .len()
        .checked_add(chunk.len())
        .ok_or(ImageError::SourceTooLarge)?;
    if next_len > max_bytes {
        return Err(ImageError::SourceTooLarge);
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

fn image_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        image_http_client_builder()
            .build()
            .expect("image HTTP client configuration is valid")
    })
}

fn guarded_image_http_client(host: &str, addrs: &[SocketAddr]) -> Result<Client, ImageError> {
    image_http_client_builder()
        .resolve_to_addrs(host, addrs)
        .build()
        .map_err(|_| ImageError::TransformFailed)
}

fn image_http_client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
}

async fn write_image_error(session: &mut Session, status: u16, body: &str) -> Result<bool> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("Cache-Control", IMAGE_ERROR_CACHE_CONTROL)?;
    header.insert_header("Content-Type", "text/plain")?;
    header.insert_header("Content-Length", body.len().to_string())?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(Bytes::from(body.to_string())), true)
        .await?;
    Ok(true)
}

fn image_error_status(error: &ImageError) -> u16 {
    match error {
        ImageError::InvalidUrl
        | ImageError::InvalidSource
        | ImageError::InvalidWidth
        | ImageError::InvalidHeight
        | ImageError::InvalidResize
        | ImageError::InvalidQuality
        | ImageError::InvalidBrowserCacheMaxAge => 400,
        ImageError::InvalidSignature | ImageError::Expired => 403,
        ImageError::SourceTooLarge | ImageError::ImageTooLarge => 413,
        ImageError::UnsupportedFormat => 415,
        ImageError::TransformFailed => 502,
    }
}

fn image_error_body(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn image_etag(path: &str, content_type: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(b"\n");
    hasher.update(content_type.as_bytes());
    format!("\"{}\"", hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    #[test]
    fn identifies_image_request_paths() {
        assert!(is_image_request_path("/_tako/image"));
        assert!(is_image_request_path("/_tako/image/v1/payload.sig"));
        assert!(!is_image_request_path("/_tako/image/v1"));
        assert!(!is_image_request_path("/_tako/channels/chat"));
    }

    #[test]
    fn image_errors_map_to_public_safe_status_codes() {
        assert_eq!(image_error_status(&ImageError::InvalidSignature), 403);
        assert_eq!(image_error_status(&ImageError::SourceTooLarge), 413);
        assert_eq!(image_error_status(&ImageError::UnsupportedFormat), 415);
    }

    #[test]
    fn response_body_chunks_stop_at_source_limit() {
        let mut bytes = vec![0_u8; 4];

        let err = append_limited_body_chunk(&mut bytes, &[1, 2, 3], 6).unwrap_err();

        assert_eq!(err, ImageError::SourceTooLarge);
        assert_eq!(bytes.len(), 4);
    }

    #[test]
    fn private_resolved_remote_addrs_are_rejected() {
        let private_addr = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 80));

        let err = validate_remote_resolved_addrs(&[private_addr]).unwrap_err();

        assert_eq!(err, ImageError::InvalidSource);
    }
}
