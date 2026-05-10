use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader, Limits};
use sha2::Sha256;
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
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
    source_content_type: Option<&str>,
    width: u32,
    quality: u8,
    limits: &TransformLimits,
) -> Result<TransformedImage, ImageError> {
    validate_width(width)?;
    validate_quality(quality)?;
    if source.len() > limits.max_source_bytes {
        return Err(ImageError::SourceTooLarge);
    }

    let format = source_format(source, source_content_type)?;
    let output_format = match format {
        ImageFormat::Png => OutputFormat::Png,
        ImageFormat::Jpeg => OutputFormat::Jpeg,
        _ => return Err(ImageError::UnsupportedFormat),
    };

    let (source_width, source_height) = image_dimensions(source, format, limits)?;
    enforce_dimension_limits(source_width, source_height, limits)?;

    let mut reader = ImageReader::with_format(Cursor::new(source), format);
    reader.limits(image_decode_limits(limits));
    let decoded = reader.decode().map_err(decode_error)?;

    if source_width == 0 || source_height == 0 {
        return Err(ImageError::TransformFailed);
    }

    let target_width = width.min(source_width);
    let target_height = (((source_height as u64) * (target_width as u64)
        + (source_width as u64 / 2))
        / source_width as u64)
        .max(1) as u32;
    let resized = decoded.resize_exact(target_width, target_height, FilterType::Lanczos3);

    let mut bytes = Vec::new();
    match output_format {
        OutputFormat::Png => resized
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|_| ImageError::TransformFailed)?,
        OutputFormat::Jpeg => {
            let rgb = resized.to_rgb8();
            let encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
            rgb.write_with_encoder(encoder)
                .map_err(|_| ImageError::TransformFailed)?;
        }
    }

    Ok(TransformedImage {
        bytes,
        content_type: output_format.content_type(),
        format: output_format,
        width: target_width,
        height: target_height,
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

fn source_format(
    source: &[u8],
    source_content_type: Option<&str>,
) -> Result<ImageFormat, ImageError> {
    if let Some(content_type) = source_content_type {
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match mime.as_str() {
            "image/png" => return Ok(ImageFormat::Png),
            "image/jpeg" | "image/jpg" => return Ok(ImageFormat::Jpeg),
            "image/webp" => return Ok(ImageFormat::WebP),
            _ => return Err(ImageError::UnsupportedFormat),
        }
    }

    image::guess_format(source).map_err(|_| ImageError::UnsupportedFormat)
}

fn image_dimensions(
    source: &[u8],
    format: ImageFormat,
    limits: &TransformLimits,
) -> Result<(u32, u32), ImageError> {
    let mut reader = ImageReader::with_format(Cursor::new(source), format);
    reader.limits(image_decode_limits(limits));
    reader.into_dimensions().map_err(decode_error)
}

fn image_decode_limits(limits: &TransformLimits) -> Limits {
    let mut decode_limits = Limits::default();
    decode_limits.max_image_width = Some(limits.max_image_width);
    decode_limits.max_image_height = Some(limits.max_image_height);
    decode_limits.max_alloc = Some(limits.max_decoded_pixels.saturating_mul(4));
    decode_limits
}

fn decode_error(error: image::ImageError) -> ImageError {
    match error {
        image::ImageError::Limits(_) => ImageError::ImageTooLarge,
        image::ImageError::Unsupported(_) => ImageError::UnsupportedFormat,
        _ => ImageError::TransformFailed,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, ImageFormat, Rgba};
    use std::io::Cursor;

    const SECRET: &str = "test-image-secret";

    fn private_options() -> ImageUrlOptions {
        ImageUrlOptions {
            source: "/assets/avatar.png".to_string(),
            width: 640,
            quality: 75,
            visibility: ImageVisibility::Private,
            expires_at_unix_secs: Some(1_900_000_000),
        }
    }

    #[test]
    fn signs_path_based_image_urls() {
        let path = sign_image_path(SECRET, &private_options()).expect("sign path");

        assert!(path.starts_with("/_tako/image/v1/private/640/75/1900000000/"));
        assert!(!path.contains('?'));
    }

    #[test]
    fn verifies_signed_private_path() {
        let path = sign_image_path(SECRET, &private_options()).expect("sign path");

        let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

        assert_eq!(verified.width, 640);
        assert_eq!(verified.quality, 75);
        assert_eq!(verified.visibility, ImageVisibility::Private);
        assert_eq!(
            verified.source,
            ImageSource::LocalPath("/assets/avatar.png".to_string())
        );
        assert_eq!(verified.expires_at_unix_secs, Some(1_900_000_000));
    }

    #[test]
    fn public_paths_do_not_require_an_expiration() {
        let mut options = private_options();
        options.visibility = ImageVisibility::Public;
        options.expires_at_unix_secs = None;

        let path = sign_image_path(SECRET, &options).expect("sign path");
        let verified = verify_image_path(SECRET, &path, u64::MAX).expect("verify path");

        assert_eq!(verified.visibility, ImageVisibility::Public);
        assert_eq!(verified.expires_at_unix_secs, None);
    }

    #[test]
    fn rejects_tampered_paths() {
        let path = sign_image_path(SECRET, &private_options()).expect("sign path");
        let tampered = path.replace("/640/", "/750/");

        let err = verify_image_path(SECRET, &tampered, 1_800_000_000).unwrap_err();

        assert_eq!(err, ImageError::InvalidSignature);
    }

    #[test]
    fn rejects_expired_private_paths() {
        let path = sign_image_path(SECRET, &private_options()).expect("sign path");

        let err = verify_image_path(SECRET, &path, 1_900_000_001).unwrap_err();

        assert_eq!(err, ImageError::Expired);
    }

    #[test]
    fn rejects_widths_outside_the_allowed_set() {
        let mut options = private_options();
        options.width = 641;

        let err = sign_image_path(SECRET, &options).unwrap_err();

        assert_eq!(err, ImageError::InvalidWidth);
    }

    #[test]
    fn rejects_invalid_quality() {
        let mut options = private_options();
        options.quality = 0;

        let err = sign_image_path(SECRET, &options).unwrap_err();

        assert_eq!(err, ImageError::InvalidQuality);
    }

    #[test]
    fn rejects_recursive_local_sources() {
        let mut options = private_options();
        options.source = "/_tako/image/v1/private/640/75/-/sig/source".to_string();

        let err = sign_image_path(SECRET, &options).unwrap_err();

        assert_eq!(err, ImageError::InvalidSource);
    }

    #[test]
    fn rejects_remote_sources_with_userinfo_or_fragments() {
        for source in [
            "https://user@example.com/avatar.png",
            "https://example.com/avatar.png#fragment",
        ] {
            let mut options = private_options();
            options.source = source.to_string();

            let err = sign_image_path(SECRET, &options).unwrap_err();

            assert_eq!(err, ImageError::InvalidSource);
        }
    }

    #[test]
    fn rejects_remote_sources_with_private_or_local_hosts() {
        for source in [
            "http://127.0.0.1/avatar.png",
            "http://[::1]/avatar.png",
            "http://localhost/avatar.png",
            "http://assets.localhost/avatar.png",
        ] {
            let mut options = private_options();
            options.source = source.to_string();

            let err = sign_image_path(SECRET, &options).unwrap_err();

            assert_eq!(err, ImageError::InvalidSource);
        }
    }

    #[test]
    fn cache_control_is_private_by_default_and_public_by_opt_in() {
        assert_eq!(
            cache_control(ImageVisibility::Private),
            PRIVATE_CACHE_CONTROL
        );
        assert_eq!(cache_control(ImageVisibility::Public), PUBLIC_CACHE_CONTROL);
    }

    #[test]
    fn resizes_png_without_upscaling() {
        let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([255_u8, 0, 0, 255]));
        let mut source = Cursor::new(Vec::new());
        img.write_to(&mut source, ImageFormat::Png)
            .expect("encode png");

        let transformed = transform_image(
            source.get_ref(),
            Some("image/png"),
            16,
            80,
            &TransformLimits::default(),
        )
        .expect("transform image");

        assert_eq!(transformed.width, 16);
        assert_eq!(transformed.height, 8);
        assert_eq!(transformed.content_type, "image/png");
        assert!(transformed.bytes.len() < source.get_ref().len());
    }

    #[test]
    fn rejects_images_above_dimension_limits_before_transforming() {
        let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([255_u8, 0, 0, 255]));
        let mut source = Cursor::new(Vec::new());
        img.write_to(&mut source, ImageFormat::Png)
            .expect("encode png");

        let err = transform_image(
            source.get_ref(),
            Some("image/png"),
            16,
            80,
            &TransformLimits {
                max_image_width: 32,
                ..Default::default()
            },
        )
        .unwrap_err();

        assert_eq!(err, ImageError::ImageTooLarge);
    }

    #[test]
    fn rejects_source_bytes_above_limit() {
        let err = transform_image(
            &[0; 16],
            Some("image/png"),
            16,
            75,
            &TransformLimits {
                max_source_bytes: 8,
                ..Default::default()
            },
        )
        .unwrap_err();

        assert_eq!(err, ImageError::SourceTooLarge);
    }
}
