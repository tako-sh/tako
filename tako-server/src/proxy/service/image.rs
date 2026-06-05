mod cache;
mod source_cache;

use super::{BackendResolution, RequestCtx};
use crate::image_worker;
use crate::instances::internal_app_host_for_app_id;
use crate::proxy::request::{insert_body_headers, static_lookup_paths};
use crate::proxy::{StaticFileError, TakoProxy};
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use reqwest::{Client, ClientBuilder, Url, redirect::Policy};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tako_images::{
    ImageError, ImageSource, PUBLIC_IMAGE_BASE_PATH, TransformLimits, TransformOptions,
    cache_control, ip_is_private_or_local, verify_public_image_request,
};
use tokio::io::AsyncReadExt;
use tokio::net::lookup_host;
use tokio::time::timeout;

const IMAGE_ERROR_CACHE_CONTROL: &str = "private, no-store";
const IMAGE_LOG_SOURCE: &str = "images";

impl TakoProxy {
    pub(crate) async fn try_handle_image_request(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
        app_name: &str,
        path: &str,
        host: &str,
        matched_route_path: Option<&str>,
    ) -> Result<bool> {
        if !is_image_request_path(path) {
            return Ok(false);
        }
        ctx.observation.set_handler("image", "pending");
        let method = session.req_header().method.as_str();
        let is_head = method == "HEAD";
        if method != "GET" && !is_head {
            ctx.observation.set_handler_result("error");
            return write_image_error(session, 405, "Method Not Allowed").await;
        }

        let Some(app) = self.lb.app_manager().get_app(app_name) else {
            ctx.observation.set_handler_result("error");
            return write_image_error(session, 404, "Not Found").await;
        };
        let (app_root, images) = {
            let config = app.config.read();
            (config.path.clone(), config.images.clone())
        };

        let accept = session
            .req_header()
            .headers
            .get("accept")
            .and_then(|value| value.to_str().ok());
        let verified =
            match verify_image_request(path, session.req_header().uri.query(), accept, &images) {
                Ok(verified) => verified,
                Err(error) => {
                    ctx.observation.set_handler_result("error");
                    let status = image_error_status(&error);
                    return write_image_error(session, status, image_error_body(status)).await;
                }
            };

        let limits = TransformLimits::default();
        let source = match self
            .load_image_source(
                app_name,
                &app_root,
                &verified.source,
                host,
                matched_route_path,
                &limits,
            )
            .await
        {
            Ok(source) => source,
            Err(error) => {
                ctx.observation.set_handler_result("error");
                let status = image_error_status(&error);
                log_image_request_error(app_name, &error, status);
                return write_image_error(session, status, image_error_body(status)).await;
            }
        };

        let transform_options = TransformOptions {
            format: verified.format,
            width: verified.width,
            height: verified.height,
            fit: verified.fit,
            crop: verified.crop,
            quality: verified.quality,
        };
        let cache_root = cache::default_cache_root();
        let cache_key =
            cache::transform_cache_key(app_name, &app_root, source.bytes(), &transform_options);
        let response = match cache::read(&cache_root, &cache_key, transform_options.format).await {
            Some(cached) => {
                ctx.observation.set_handler_result("hit");
                ImageResponseBody {
                    bytes: cached.bytes,
                    content_type: cached.content_type.to_string(),
                    cacheable: true,
                }
            }
            None => match transform_uncached_image(
                app_name,
                source,
                transform_options,
                limits,
                &cache_root,
                &cache_key,
            )
            .await
            {
                Ok(response) => {
                    if response.cacheable {
                        ctx.observation.set_handler_result("miss");
                    } else {
                        ctx.observation.set_handler_result("fallback");
                    }
                    response
                }
                Err(error) => {
                    ctx.observation.set_handler_result("error");
                    let status = image_error_status(&error);
                    log_image_request_error(app_name, &error, status);
                    return write_image_error(session, status, image_error_body(status)).await;
                }
            },
        };

        let mut header = ResponseHeader::build(200, None)?;
        header.insert_header("Content-Type", response.content_type.as_str())?;
        header.insert_header("Content-Length", response.bytes.len().to_string())?;
        if response.cacheable {
            let cache_control_header =
                cache_control(verified.visibility, verified.private_browser_cache_max_age);
            header.insert_header("Cache-Control", cache_control_header.as_ref())?;
        } else {
            header.insert_header("Cache-Control", IMAGE_ERROR_CACHE_CONTROL)?;
        }
        if verified.vary_accept {
            header.insert_header("Vary", "Accept")?;
        }
        header.insert_header(
            "ETag",
            image_etag(&response.bytes, response.content_type.as_str()),
        )?;
        session
            .write_response_header(Box::new(header), false)
            .await?;

        if is_head {
            session.write_response_body(None, true).await?;
        } else {
            session
                .write_response_body(Some(Bytes::from(response.bytes)), true)
                .await?;
        }
        Ok(true)
    }

    async fn load_image_source(
        &self,
        app_name: &str,
        app_root: &Path,
        source: &ImageSource,
        host: &str,
        matched_route_path: Option<&str>,
        limits: &TransformLimits,
    ) -> Result<ImageSourceBytes, ImageError> {
        let cache_key = source_cache_key(app_name, app_root, source, host, matched_route_path);
        source_cache::get_or_load(&cache_key, || async {
            self.load_image_source_uncached(
                app_name,
                app_root,
                source,
                host,
                matched_route_path,
                limits,
            )
            .await
        })
        .await
    }

    async fn load_image_source_uncached(
        &self,
        app_name: &str,
        app_root: &Path,
        source: &ImageSource,
        host: &str,
        matched_route_path: Option<&str>,
        limits: &TransformLimits,
    ) -> Result<ImageSourceBytes, ImageError> {
        match source {
            ImageSource::LocalPath(path) => {
                if let Some(source) = self
                    .load_static_image_source(app_name, app_root, path, matched_route_path, limits)
                    .await?
                {
                    return Ok(source);
                }
                self.fetch_backend_image_source(app_name, path, host, limits)
                    .await
            }
            ImageSource::RemoteUrl(url) => fetch_remote_image_source(url, limits).await,
        }
    }

    async fn load_static_image_source(
        &self,
        app_name: &str,
        app_root: &Path,
        path: &str,
        matched_route_path: Option<&str>,
        limits: &TransformLimits,
    ) -> Result<Option<ImageSourceBytes>, ImageError> {
        let static_server = self.static_server_for_app(app_name, app_root);
        if !static_server.is_available() {
            return Ok(None);
        }

        for lookup_path in static_lookup_paths(path, matched_route_path) {
            match static_server.resolve(&lookup_path) {
                Ok(file) => {
                    let bytes = read_file_limited(&file.path, limits.max_source_bytes).await?;
                    return Ok(Some(ImageSourceBytes::new(bytes, Some(file.content_type))));
                }
                Err(StaticFileError::NotFound(_)) | Err(StaticFileError::Io(_)) => {}
                Err(StaticFileError::PathTraversal(_)) | Err(StaticFileError::InvalidPath(_)) => {
                    return Err(ImageError::InvalidSource);
                }
            }
        }

        Ok(None)
    }

    async fn fetch_backend_image_source(
        &self,
        app_name: &str,
        path: &str,
        host: &str,
        limits: &TransformLimits,
    ) -> Result<ImageSourceBytes, ImageError> {
        let backend = match self.resolve_backend(app_name).await {
            BackendResolution::Ready { backend, .. } => backend,
            _ => return Err(ImageError::TransformFailed),
        };
        let endpoint = backend.endpoint().ok_or(ImageError::TransformFailed)?;
        let url = format!("http://{endpoint}{path}");
        let internal_host = internal_app_host_for_app_id(app_name);
        let host = if host.is_empty() {
            internal_host.as_str()
        } else {
            host
        };
        backend.request_started();
        let result = fetch_image_source(image_http_client(), &url, Some(host), limits).await;
        backend.request_finished();
        result
    }
}

#[derive(Clone)]
struct ImageSourceBytes {
    bytes: Arc<[u8]>,
    content_type: Option<String>,
}

impl ImageSourceBytes {
    fn new(bytes: Vec<u8>, content_type: Option<String>) -> Self {
        Self {
            bytes: Arc::from(bytes.into_boxed_slice()),
            content_type,
        }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }

    fn to_vec(&self) -> Vec<u8> {
        self.bytes.to_vec()
    }
}

#[derive(Debug)]
struct ImageResponseBody {
    bytes: Vec<u8>,
    content_type: String,
    cacheable: bool,
}

impl ImageResponseBody {
    fn from_transformed(transformed: tako_images::TransformedImage) -> Self {
        Self {
            bytes: transformed.bytes,
            content_type: transformed.content_type.to_string(),
            cacheable: true,
        }
    }
}

enum TransformImageOutcome {
    Transformed(tako_images::TransformedImage),
    Failed(ImageError, ImageSourceBytes),
}

async fn transform_uncached_image(
    app_name: &str,
    source: ImageSourceBytes,
    options: TransformOptions,
    limits: TransformLimits,
    cache_root: &Path,
    cache_key: &str,
) -> Result<ImageResponseBody, ImageError> {
    loop {
        match cache::acquire_transform_lease(cache_key) {
            cache::TransformLease::Owner(_lease) => {
                if let Some(cached) = cache::read(cache_root, cache_key, options.format).await {
                    return Ok(ImageResponseBody {
                        bytes: cached.bytes,
                        content_type: cached.content_type.to_string(),
                        cacheable: true,
                    });
                }
                return match transform_image_isolated(app_name, source, options, limits).await {
                    TransformImageOutcome::Transformed(transformed) => {
                        cache::write(cache_root, cache_key, &transformed.bytes).await;
                        Ok(ImageResponseBody::from_transformed(transformed))
                    }
                    TransformImageOutcome::Failed(error, source) => {
                        image_response_body_from_transform_error(app_name, error, source, options)
                    }
                };
            }
            cache::TransformLease::Waiter(waiter) => {
                waiter.wait().await;
                if let Some(cached) = cache::read(cache_root, cache_key, options.format).await {
                    return Ok(ImageResponseBody {
                        bytes: cached.bytes,
                        content_type: cached.content_type.to_string(),
                        cacheable: true,
                    });
                }
            }
        }
    }
}

async fn transform_image_isolated(
    app_name: &str,
    source: ImageSourceBytes,
    options: TransformOptions,
    limits: TransformLimits,
) -> TransformImageOutcome {
    match image_worker::transform_in_worker(
        app_name,
        source.bytes(),
        source.content_type.as_deref(),
        options,
        &limits,
    )
    .await
    {
        Ok(transformed) => TransformImageOutcome::Transformed(transformed),
        Err(error) => TransformImageOutcome::Failed(error, source),
    }
}

fn image_response_body_from_transform_error(
    app_name: &str,
    error: ImageError,
    source: ImageSourceBytes,
    options: TransformOptions,
) -> Result<ImageResponseBody, ImageError> {
    if error != ImageError::TransformFailed {
        return Err(error);
    }
    let source_bytes = source.len();
    let content_type = source
        .content_type
        .as_ref()
        .filter(|content_type| is_image_content_type(content_type))
        .cloned()
        .ok_or(ImageError::TransformFailed)?;

    tracing::warn!(
        app = %app_name,
        source = IMAGE_LOG_SOURCE,
        error = %error,
        requested_format = ?options.format,
        width = options.width,
        height = ?options.height,
        quality = options.quality,
        source_bytes = source_bytes as u64,
        content_type = %content_type,
        "Image transform failed; serving original image"
    );

    Ok(ImageResponseBody {
        bytes: source.to_vec(),
        content_type,
        cacheable: false,
    })
}

fn log_image_request_error(app_name: &str, error: &ImageError, status: u16) {
    if status < 500 {
        return;
    }

    tracing::warn!(
        app = %app_name,
        source = IMAGE_LOG_SOURCE,
        error = %error,
        status,
        "Image optimizer request failed"
    );
}

fn is_image_content_type(content_type: &str) -> bool {
    let Some(media_type) = content_type.split(';').next().map(str::trim) else {
        return false;
    };
    media_type.len() > "image/".len()
        && media_type
            .get(.."image/".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
}

fn is_image_request_path(path: &str) -> bool {
    path == PUBLIC_IMAGE_BASE_PATH
}

fn verify_image_request(
    path: &str,
    query: Option<&str>,
    accept: Option<&str>,
    images: &tako_images::ImagesConfig,
) -> Result<tako_images::VerifiedImageRequest, ImageError> {
    verify_public_image_request(path, query, accept, images)
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
    Ok(ImageSourceBytes::new(bytes, content_type))
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
    CLIENT.get_or_init(|| build_image_http_client(image_http_client_builder()))
}

fn guarded_image_http_client(host: &str, addrs: &[SocketAddr]) -> Result<Client, ImageError> {
    image_http_client_builder()
        .resolve_to_addrs(host, addrs)
        .build()
        .map_err(|_| ImageError::TransformFailed)
}

fn build_image_http_client(builder: ClientBuilder) -> Client {
    builder
        .build()
        .expect("image HTTP client configuration is valid")
}

fn image_http_client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
}

async fn read_file_limited(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ImageError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ImageError::InvalidSource)?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|_| ImageError::InvalidSource)?;
        if read == 0 {
            return Ok(bytes);
        }
        if bytes.len() + read > max_bytes {
            return Err(ImageError::SourceTooLarge);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

async fn write_image_error(session: &mut Session, status: u16, body: &str) -> Result<bool> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("Cache-Control", IMAGE_ERROR_CACHE_CONTROL)?;
    insert_body_headers(&mut header, "text/plain", body)?;
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
        ImageError::TransformQueueFull => 503,
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
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn image_etag(bytes: &[u8], content_type: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"tako-image-response-v1");
    hasher.update(b"\n");
    hasher.update(content_type.as_bytes());
    hasher.update(b"\n");
    hasher.update(bytes);
    format!("\"{}\"", hex::encode(hasher.finalize()))
}

fn source_cache_key(
    app_name: &str,
    app_root: &Path,
    source: &ImageSource,
    host: &str,
    matched_route_path: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"tako-image-source-v1");
    hasher.update(b"\napp\n");
    hasher.update(app_name.as_bytes());
    hasher.update(b"\nroot\n");
    hasher.update(app_root.to_string_lossy().as_bytes());
    match source {
        ImageSource::LocalPath(path) => {
            hasher.update(b"\nlocal\n");
            hasher.update(path.as_bytes());
            hasher.update(b"\nhost\n");
            hasher.update(host.as_bytes());
            hasher.update(b"\nroute\n");
            hasher.update(matched_route_path.unwrap_or("").as_bytes());
        }
        ImageSource::RemoteUrl(url) => {
            hasher.update(b"\nremote\n");
            hasher.update(url.as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests;
