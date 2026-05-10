use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use libvips::ops::{
    ForeignKeep, ForeignPngFilter, ForeignWebpPreset, JpegsaveBufferOptions, PngsaveBufferOptions,
    WebpsaveBufferOptions,
};
use libvips::{VipsApp, VipsImage, ops};
use sha2::Sha256;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;
use std::time::Duration;
use url::{Host, Url};

pub const IMAGE_BASE_PATH: &str = "/_tako/image/v1";
pub const DEFAULT_PRIVATE_MAX_AGE: Duration = Duration::from_secs(86_400);
pub const PUBLIC_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
pub const PRIVATE_CACHE_CONTROL: &str = "private, max-age=86400";

const MAX_SOURCE_CHARS: usize = 2048;
const ALLOWED_WIDTHS: &[u32] = &[
    16, 32, 48, 64, 96, 128, 256, 384, 640, 750, 828, 1080, 1200, 1920, 2048, 3840,
];

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageVisibility {
    Public,
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageUrlOptions {
    pub source: String,
    pub width: u32,
    pub quality: u8,
    pub visibility: ImageVisibility,
    pub expires_at_unix_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedImageRequest {
    pub source: ImageSource,
    pub width: u32,
    pub quality: u8,
    pub visibility: ImageVisibility,
    pub expires_at_unix_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    LocalPath(String),
    RemoteUrl(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformLimits {
    pub max_source_bytes: usize,
    pub max_image_width: u32,
    pub max_image_height: u32,
    pub max_decoded_pixels: u64,
}

impl Default for TransformLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 8 * 1024 * 1024,
            max_image_width: 8_000,
            max_image_height: 8_000,
            max_decoded_pixels: 32_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Png,
    Jpeg,
    Webp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformedImage {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub format: OutputFormat,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImageError {
    #[error("invalid image URL")]
    InvalidUrl,
    #[error("invalid image source")]
    InvalidSource,
    #[error("invalid image width")]
    InvalidWidth,
    #[error("invalid image quality")]
    InvalidQuality,
    #[error("invalid image signature")]
    InvalidSignature,
    #[error("image URL has expired")]
    Expired,
    #[error("image source is too large")]
    SourceTooLarge,
    #[error("unsupported image format")]
    UnsupportedFormat,
    #[error("image exceeds safety limits")]
    ImageTooLarge,
    #[error("image transform failed")]
    TransformFailed,
}

pub fn sign_image_path(secret: &str, options: &ImageUrlOptions) -> Result<String, ImageError> {
    validate_width(options.width)?;
    validate_quality(options.quality)?;
    let source = parse_source(&options.source)?;
    let expires = expires_segment(options.expires_at_unix_secs);
    let visibility = visibility_segment(options.visibility);
    let source_raw = source.as_str();
    let signature = signature(
        secret,
        visibility,
        options.width,
        options.quality,
        &expires,
        source_raw,
    )?;
    let encoded_source = URL_SAFE_NO_PAD.encode(source_raw.as_bytes());

    Ok(format!(
        "{IMAGE_BASE_PATH}/{visibility}/{}/{}/{expires}/{signature}/{encoded_source}",
        options.width, options.quality
    ))
}

pub fn verify_image_path(
    secret: &str,
    path: &str,
    now_unix_secs: u64,
) -> Result<VerifiedImageRequest, ImageError> {
    let parts = parse_path(path)?;
    validate_width(parts.width)?;
    validate_quality(parts.quality)?;

    let expected = signature(
        secret,
        visibility_segment(parts.visibility),
        parts.width,
        parts.quality,
        &expires_segment(parts.expires_at_unix_secs),
        &parts.source,
    )?;
    if !constant_time_eq(expected.as_bytes(), parts.signature.as_bytes()) {
        return Err(ImageError::InvalidSignature);
    }

    if let Some(expires_at) = parts.expires_at_unix_secs
        && now_unix_secs > expires_at
    {
        return Err(ImageError::Expired);
    }

    Ok(VerifiedImageRequest {
        source: parse_source(&parts.source)?,
        width: parts.width,
        quality: parts.quality,
        visibility: parts.visibility,
        expires_at_unix_secs: parts.expires_at_unix_secs,
    })
}

pub fn cache_control(visibility: ImageVisibility) -> &'static str {
    match visibility {
        ImageVisibility::Public => PUBLIC_CACHE_CONTROL,
        ImageVisibility::Private => PRIVATE_CACHE_CONTROL,
    }
}

pub fn transform_image(
    source: &[u8],
    _source_content_type: Option<&str>,
    width: u32,
    quality: u8,
    limits: &TransformLimits,
) -> Result<TransformedImage, ImageError> {
    validate_width(width)?;
    validate_quality(quality)?;
    if source.len() > limits.max_source_bytes {
        return Err(ImageError::SourceTooLarge);
    }

    let format = source_format(source)?;
    let (source_width, source_height) = image_dimensions(source)?;
    enforce_dimension_limits(source_width, source_height, limits)?;

    if source_width == 0 || source_height == 0 {
        return Err(ImageError::TransformFailed);
    }

    let target_width = width.min(source_width);
    let resized = if target_width == source_width {
        autorotate_image(&read_image_header(source)?)?
    } else {
        thumbnail_image(source, target_width)?
    };
    let width = dimension_from_vips(resized.get_width())?;
    let height = dimension_from_vips(resized.get_height())?;
    let bytes = encode_image(&resized, format, quality)?;

    Ok(TransformedImage {
        bytes,
        content_type: format.content_type(),
        format,
        width,
        height,
    })
}

impl ImageVisibility {
    fn parse(value: &str) -> Result<Self, ImageError> {
        match value {
            "public" => Ok(Self::Public),
            "private" => Ok(Self::Private),
            _ => Err(ImageError::InvalidUrl),
        }
    }
}

impl ImageSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::LocalPath(path) | Self::RemoteUrl(path) => path,
        }
    }
}

impl OutputFormat {
    fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
}

struct ParsedPath {
    source: String,
    width: u32,
    quality: u8,
    visibility: ImageVisibility,
    expires_at_unix_secs: Option<u64>,
    signature: String,
}

fn parse_path(path: &str) -> Result<ParsedPath, ImageError> {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() != 10
        || !segments[0].is_empty()
        || segments[1] != "_tako"
        || segments[2] != "image"
        || segments[3] != "v1"
    {
        return Err(ImageError::InvalidUrl);
    }

    let visibility = ImageVisibility::parse(segments[4])?;
    let width = segments[5]
        .parse::<u32>()
        .map_err(|_| ImageError::InvalidWidth)?;
    let quality = segments[6]
        .parse::<u8>()
        .map_err(|_| ImageError::InvalidQuality)?;
    let expires_at_unix_secs = parse_expires_segment(segments[7])?;
    let signature = segments[8].to_string();
    if signature.is_empty() {
        return Err(ImageError::InvalidSignature);
    }
    let source = URL_SAFE_NO_PAD
        .decode(segments[9])
        .map_err(|_| ImageError::InvalidUrl)?;
    let source = String::from_utf8(source).map_err(|_| ImageError::InvalidSource)?;

    Ok(ParsedPath {
        source,
        width,
        quality,
        visibility,
        expires_at_unix_secs,
        signature,
    })
}

fn validate_width(width: u32) -> Result<(), ImageError> {
    if ALLOWED_WIDTHS.contains(&width) {
        Ok(())
    } else {
        Err(ImageError::InvalidWidth)
    }
}

fn validate_quality(quality: u8) -> Result<(), ImageError> {
    if (1..=100).contains(&quality) {
        Ok(())
    } else {
        Err(ImageError::InvalidQuality)
    }
}

fn parse_source(source: &str) -> Result<ImageSource, ImageError> {
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
        if source.starts_with("//") || source.starts_with(IMAGE_BASE_PATH) {
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

fn visibility_segment(visibility: ImageVisibility) -> &'static str {
    match visibility {
        ImageVisibility::Public => "public",
        ImageVisibility::Private => "private",
    }
}

fn expires_segment(expires_at_unix_secs: Option<u64>) -> String {
    expires_at_unix_secs.map_or_else(|| "-".to_string(), |value| value.to_string())
}

fn parse_expires_segment(value: &str) -> Result<Option<u64>, ImageError> {
    if value == "-" {
        return Ok(None);
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| ImageError::InvalidUrl)
}

fn signature(
    secret: &str,
    visibility: &str,
    width: u32,
    quality: u8,
    expires: &str,
    source: &str,
) -> Result<String, ImageError> {
    if secret.is_empty() {
        return Err(ImageError::InvalidSignature);
    }
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| ImageError::InvalidSignature)?;
    mac.update(b"v1\n");
    mac.update(visibility.as_bytes());
    mac.update(b"\n");
    mac.update(width.to_string().as_bytes());
    mac.update(b"\n");
    mac.update(quality.to_string().as_bytes());
    mac.update(b"\n");
    mac.update(expires.as_bytes());
    mac.update(b"\n");
    mac.update(source.as_bytes());
    let bytes = mac.finalize().into_bytes();
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn source_format(source: &[u8]) -> Result<OutputFormat, ImageError> {
    if source.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Ok(OutputFormat::Png);
    }
    if source.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok(OutputFormat::Jpeg);
    }
    if source.len() >= 12 && &source[0..4] == b"RIFF" && &source[8..12] == b"WEBP" {
        return Ok(OutputFormat::Webp);
    }

    Err(ImageError::UnsupportedFormat)
}

fn image_dimensions(source: &[u8]) -> Result<(u32, u32), ImageError> {
    let image = read_image_header(source)?;
    Ok((
        dimension_from_vips(image.get_width())?,
        dimension_from_vips(image.get_height())?,
    ))
}

fn enforce_dimension_limits(
    width: u32,
    height: u32,
    limits: &TransformLimits,
) -> Result<(), ImageError> {
    if width > limits.max_image_width || height > limits.max_image_height {
        return Err(ImageError::ImageTooLarge);
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > limits.max_decoded_pixels {
        return Err(ImageError::ImageTooLarge);
    }
    Ok(())
}

fn read_image_header(source: &[u8]) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    VipsImage::new_from_buffer(source, "").map_err(|_| vips_transform_error(app))
}

fn autorotate_image(image: &VipsImage) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    ops::autorot(image).map_err(|_| vips_transform_error(app))
}

fn thumbnail_image(source: &[u8], width: u32) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    ops::thumbnail_buffer(
        source,
        i32::try_from(width).map_err(|_| ImageError::InvalidWidth)?,
    )
    .map_err(|_| vips_transform_error(app))
}

fn encode_image(
    image: &VipsImage,
    format: OutputFormat,
    quality: u8,
) -> Result<Vec<u8>, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    match format {
        OutputFormat::Png => ops::pngsave_buffer_with_opts(
            image,
            &PngsaveBufferOptions {
                compression: 9,
                filter: ForeignPngFilter::All,
                keep: ForeignKeep::None,
                ..PngsaveBufferOptions::default()
            },
        ),
        OutputFormat::Jpeg => ops::jpegsave_buffer_with_opts(
            image,
            &JpegsaveBufferOptions {
                q: i32::from(quality),
                optimize_coding: true,
                interlace: true,
                keep: ForeignKeep::None,
                ..JpegsaveBufferOptions::default()
            },
        ),
        OutputFormat::Webp => ops::webpsave_buffer_with_opts(
            image,
            &WebpsaveBufferOptions {
                q: i32::from(quality),
                alpha_q: i32::from(quality),
                smart_subsample: true,
                preset: ForeignWebpPreset::Photo,
                keep: ForeignKeep::None,
                ..WebpsaveBufferOptions::default()
            },
        ),
    }
    .map_err(|_| vips_transform_error(app))
}

fn vips_transform_error(_app: &VipsApp) -> ImageError {
    #[cfg(test)]
    if let Ok(error) = _app.error_buffer()
        && !error.is_empty()
    {
        eprintln!("libvips error: {error}");
    }
    ImageError::TransformFailed
}

fn dimension_from_vips(value: i32) -> Result<u32, ImageError> {
    u32::try_from(value).map_err(|_| ImageError::TransformFailed)
}

fn vips_app() -> Result<&'static VipsApp, ImageError> {
    static APP: OnceLock<Result<VipsApp, ()>> = OnceLock::new();
    match APP.get_or_init(|| {
        let app = VipsApp::new("tako-images", false).map_err(|_| ())?;
        app.cache_set_max(100);
        app.cache_set_max_mem(128 * 1024 * 1024);
        Ok(app)
    }) {
        Ok(app) => Ok(app),
        Err(()) => Err(ImageError::TransformFailed),
    }
}

#[cfg(test)]
mod tests;
