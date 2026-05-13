use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use libvips::{VipsApp, VipsImage, ops};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::borrow::Cow;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;
use std::time::Duration;
use url::{Host, Url};

pub const IMAGE_BASE_PATH: &str = "/_tako/image/v1";
pub const PUBLIC_IMAGE_BASE_PATH: &str = "/_tako/image";
pub const DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE: Duration = Duration::from_secs(604_800);
pub const MAX_PRIVATE_BROWSER_CACHE_MAX_AGE: Duration = Duration::from_secs(31_536_000);
pub const PUBLIC_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
pub const PRIVATE_CACHE_CONTROL: &str = "private, max-age=604800";

const MAX_SOURCE_CHARS: usize = 2048;
const ALLOWED_DIMENSIONS: &[u32] = &[
    16, 32, 48, 64, 96, 128, 256, 384, 640, 750, 828, 1080, 1200, 1920, 2048, 3840,
];
const DEFAULT_WIDTH: u32 = 1200;
const DEFAULT_QUALITY: u8 = 75;
const STRIP_SOURCE_METADATA: &str = "strip";
const DEFAULT_PUBLIC_WIDTHS: &[u32] = &[320, 640, 960, 1200, 1920];
const DEFAULT_PUBLIC_QUALITIES: &[u8] = &[75];
const DEFAULT_PUBLIC_FORMATS: &[OutputFormat] = &[OutputFormat::Avif, OutputFormat::Webp];

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageVisibility {
    Public,
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageUrlOptions {
    pub source: String,
    pub format: OutputFormat,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fit: Option<ImageFit>,
    pub crop: Option<ImageCrop>,
    pub quality: u8,
    pub visibility: ImageVisibility,
    pub expires_at_unix_secs: Option<u64>,
    pub private_browser_cache_max_age: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedImageRequest {
    pub source: ImageSource,
    pub format: OutputFormat,
    pub width: u32,
    pub height: Option<u32>,
    pub fit: Option<ImageFit>,
    pub crop: Option<ImageCrop>,
    pub quality: u8,
    pub visibility: ImageVisibility,
    pub expires_at_unix_secs: Option<u64>,
    pub private_browser_cache_max_age: Option<Duration>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ImagesConfig {
    pub local_patterns: Option<Vec<String>>,
    pub remote_patterns: Vec<String>,
    pub sizes: Vec<u32>,
    pub qualities: Vec<u8>,
    pub formats: Vec<OutputFormat>,
}

impl Default for ImagesConfig {
    fn default() -> Self {
        Self {
            local_patterns: None,
            remote_patterns: Vec::new(),
            sizes: DEFAULT_PUBLIC_WIDTHS.to_vec(),
            qualities: DEFAULT_PUBLIC_QUALITIES.to_vec(),
            formats: DEFAULT_PUBLIC_FORMATS.to_vec(),
        }
    }
}

impl ImagesConfig {
    pub fn validate(&self) -> Result<(), ImageError> {
        let default_local_patterns = ["/**".to_string()];
        validate_pattern_list(
            self.local_patterns
                .as_deref()
                .unwrap_or(&default_local_patterns),
            true,
        )?;
        validate_pattern_list(&self.remote_patterns, false)?;
        if self.sizes.is_empty() || self.sizes.contains(&0) {
            return Err(ImageError::InvalidWidth);
        }
        if self.qualities.is_empty()
            || self
                .qualities
                .iter()
                .any(|quality| !(1..=100).contains(quality))
        {
            return Err(ImageError::InvalidQuality);
        }
        if self.formats.is_empty() {
            return Err(ImageError::UnsupportedFormat);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Avif,
    Webp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFit {
    Cover,
    Contain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageCrop {
    Center,
    Smart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformedImage {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub format: OutputFormat,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransformOptions {
    pub format: OutputFormat,
    pub width: u32,
    pub height: Option<u32>,
    pub fit: Option<ImageFit>,
    pub crop: Option<ImageCrop>,
    pub quality: u8,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImageError {
    #[error("invalid image URL")]
    InvalidUrl,
    #[error("invalid image source")]
    InvalidSource,
    #[error("invalid image width")]
    InvalidWidth,
    #[error("invalid image height")]
    InvalidHeight,
    #[error("invalid image resize options")]
    InvalidResize,
    #[error("invalid image quality")]
    InvalidQuality,
    #[error("invalid private browser cache max-age")]
    InvalidBrowserCacheMaxAge,
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
    validate_quality(options.quality)?;
    let resize = normalize_resize(options.width, options.height, options.fit, options.crop)?;
    let source = parse_source(&options.source)?;
    let (expires_at_unix_secs, private_browser_cache_max_age) = match options.visibility {
        ImageVisibility::Public => {
            if options.expires_at_unix_secs.is_some() {
                return Err(ImageError::InvalidUrl);
            }
            if options.private_browser_cache_max_age.is_some() {
                return Err(ImageError::InvalidBrowserCacheMaxAge);
            }
            (None, None)
        }
        ImageVisibility::Private => {
            let max_age = options
                .private_browser_cache_max_age
                .unwrap_or(DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE);
            validate_private_browser_cache_max_age(max_age)?;
            (
                Some(options.expires_at_unix_secs.ok_or(ImageError::InvalidUrl)?),
                Some(max_age),
            )
        }
    };
    let source_raw = source.as_str();
    let payload = ImagePayload {
        public: (options.visibility == ImageVisibility::Public).then_some(true),
        format: (options.format != OutputFormat::Avif)
            .then(|| options.format.payload_value().to_string()),
        width: (resize.width != DEFAULT_WIDTH || resize.height.is_some()).then_some(resize.width),
        height: resize.height,
        fit: resize
            .fit
            .and_then(|fit| (fit != ImageFit::Cover).then(|| fit.payload_value().to_string())),
        crop: resize
            .crop
            .and_then(|crop| (crop != ImageCrop::Center).then(|| crop.payload_value().to_string())),
        quality: (options.quality != DEFAULT_QUALITY).then_some(options.quality),
        private_browser_cache_max_age_secs: private_browser_cache_max_age
            .filter(|max_age| *max_age != DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE)
            .map(|max_age| max_age.as_secs()),
        expires_at_unix_secs,
        source: source_raw.to_string(),
    };
    let encoded_payload = encode_payload(&payload)?;
    let signature = signature(secret, &encoded_payload)?;

    Ok(format!("{IMAGE_BASE_PATH}/{encoded_payload}.{signature}"))
}

pub fn verify_image_path(
    secret: &str,
    path: &str,
    now_unix_secs: u64,
) -> Result<VerifiedImageRequest, ImageError> {
    let parts = parse_path(path)?;
    validate_width(parts.width)?;
    if let Some(height) = parts.height {
        validate_height(height)?;
    }
    validate_quality(parts.quality)?;

    let expected = signature(secret, &parts.encoded_payload)?;
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
        format: parts.format,
        width: parts.width,
        height: parts.height,
        fit: parts.fit,
        crop: parts.crop,
        quality: parts.quality,
        visibility: parts.visibility,
        expires_at_unix_secs: parts.expires_at_unix_secs,
        private_browser_cache_max_age: parts.private_browser_cache_max_age,
    })
}

pub fn verify_public_image_request(
    path: &str,
    query: Option<&str>,
    accept: Option<&str>,
    config: &ImagesConfig,
) -> Result<VerifiedImageRequest, ImageError> {
    if path != PUBLIC_IMAGE_BASE_PATH {
        return Err(ImageError::InvalidUrl);
    }
    config.validate()?;
    let query = query.ok_or(ImageError::InvalidUrl)?;
    let params = parse_public_query(query)?;
    let source = parse_source(&params.source)?;
    validate_public_source_allowed(&source, config)?;
    let width = parse_public_width(&params.width, config)?;
    let quality = match params.quality {
        Some(value) => parse_public_quality(&value, config)?,
        None => DEFAULT_QUALITY,
    };
    let format = match params.format {
        Some(value) => parse_public_format(&value, config)?,
        None => negotiate_public_format(accept, config)?,
    };

    Ok(VerifiedImageRequest {
        source,
        format,
        width,
        height: None,
        fit: None,
        crop: None,
        quality,
        visibility: ImageVisibility::Public,
        expires_at_unix_secs: None,
        private_browser_cache_max_age: None,
    })
}

pub fn cache_control(
    visibility: ImageVisibility,
    private_browser_cache_max_age: Option<Duration>,
) -> Cow<'static, str> {
    match visibility {
        ImageVisibility::Public => Cow::Borrowed(PUBLIC_CACHE_CONTROL),
        ImageVisibility::Private => {
            let max_age =
                private_browser_cache_max_age.unwrap_or(DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE);
            if max_age == DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE {
                Cow::Borrowed(PRIVATE_CACHE_CONTROL)
            } else {
                Cow::Owned(format!("private, max-age={}", max_age.as_secs()))
            }
        }
    }
}

pub fn transform_image(
    source: &[u8],
    _source_content_type: Option<&str>,
    options: TransformOptions,
    limits: &TransformLimits,
) -> Result<TransformedImage, ImageError> {
    validate_width(options.width)?;
    validate_quality(options.quality)?;
    let resize = normalize_resize(
        Some(options.width),
        options.height,
        options.fit,
        options.crop,
    )?;
    if source.len() > limits.max_source_bytes {
        return Err(ImageError::SourceTooLarge);
    }

    validate_source_format(source)?;
    let (source_width, source_height) = image_dimensions(source)?;
    enforce_dimension_limits(source_width, source_height, limits)?;

    if source_width == 0 || source_height == 0 {
        return Err(ImageError::TransformFailed);
    }

    let resized = thumbnail_image(source, options.width, resize)?;
    let width = dimension_from_vips(resized.get_width())?;
    let height = dimension_from_vips(resized.get_height())?;
    let bytes = encode_image(&resized, options.format, options.quality)?;

    Ok(TransformedImage {
        bytes,
        content_type: options.format.content_type(),
        format: options.format,
        width,
        height,
    })
}

impl ImageSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::LocalPath(path) | Self::RemoteUrl(path) => path,
        }
    }
}

impl OutputFormat {
    fn parse_payload_override(value: &str) -> Result<Self, ImageError> {
        match value {
            "webp" => Ok(Self::Webp),
            _ => Err(ImageError::UnsupportedFormat),
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Avif => "image/avif",
            Self::Webp => "image/webp",
        }
    }

    fn payload_value(self) -> &'static str {
        match self {
            Self::Avif => "avif",
            Self::Webp => "webp",
        }
    }
}

impl ImageFit {
    fn parse(value: &str) -> Result<Self, ImageError> {
        match value {
            "cover" => Ok(Self::Cover),
            "contain" => Ok(Self::Contain),
            _ => Err(ImageError::InvalidResize),
        }
    }

    fn payload_value(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
        }
    }
}

impl ImageCrop {
    fn parse(value: &str) -> Result<Self, ImageError> {
        match value {
            "center" => Ok(Self::Center),
            "smart" => Ok(Self::Smart),
            _ => Err(ImageError::InvalidResize),
        }
    }

    fn payload_value(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::Smart => "smart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NormalizedResize {
    width: u32,
    height: Option<u32>,
    fit: Option<ImageFit>,
    crop: Option<ImageCrop>,
}

struct ParsedPath {
    encoded_payload: String,
    source: String,
    format: OutputFormat,
    width: u32,
    height: Option<u32>,
    fit: Option<ImageFit>,
    crop: Option<ImageCrop>,
    quality: u8,
    visibility: ImageVisibility,
    expires_at_unix_secs: Option<u64>,
    private_browser_cache_max_age: Option<Duration>,
    signature: String,
}

struct PublicQuery {
    source: String,
    width: String,
    quality: Option<String>,
    format: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImagePayload {
    #[serde(rename = "pub", skip_serializing_if = "Option::is_none")]
    public: Option<bool>,
    #[serde(rename = "f", skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    #[serde(rename = "w", skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(rename = "h", skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(rename = "fit", skip_serializing_if = "Option::is_none")]
    fit: Option<String>,
    #[serde(rename = "crop", skip_serializing_if = "Option::is_none")]
    crop: Option<String>,
    #[serde(rename = "q", skip_serializing_if = "Option::is_none")]
    quality: Option<u8>,
    #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
    private_browser_cache_max_age_secs: Option<u64>,
    #[serde(rename = "e", skip_serializing_if = "Option::is_none")]
    expires_at_unix_secs: Option<u64>,
    #[serde(rename = "s")]
    source: String,
}

fn parse_path(path: &str) -> Result<ParsedPath, ImageError> {
    let token = path
        .strip_prefix(IMAGE_BASE_PATH)
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or(ImageError::InvalidUrl)?;
    if token.is_empty() || token.contains('/') {
        return Err(ImageError::InvalidUrl);
    }
    let (encoded_payload, signature) = token.split_once('.').ok_or(ImageError::InvalidUrl)?;
    if encoded_payload.is_empty() || signature.is_empty() || signature.contains('.') {
        return Err(ImageError::InvalidSignature);
    }
    let payload = decode_payload(encoded_payload)?;
    let visibility = match payload.public {
        None => ImageVisibility::Private,
        Some(true) => ImageVisibility::Public,
        Some(false) => return Err(ImageError::InvalidUrl),
    };
    let (expires_at_unix_secs, private_browser_cache_max_age) = match visibility {
        ImageVisibility::Public => {
            if payload.expires_at_unix_secs.is_some() {
                return Err(ImageError::InvalidUrl);
            }
            if payload.private_browser_cache_max_age_secs.is_some() {
                return Err(ImageError::InvalidBrowserCacheMaxAge);
            }
            (None, None)
        }
        ImageVisibility::Private => {
            let max_age = browser_cache_duration_from_payload_seconds(
                payload
                    .private_browser_cache_max_age_secs
                    .unwrap_or(DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE.as_secs()),
            )?;
            (
                Some(payload.expires_at_unix_secs.ok_or(ImageError::InvalidUrl)?),
                Some(max_age),
            )
        }
    };
    let format = payload
        .format
        .as_deref()
        .map(OutputFormat::parse_payload_override)
        .transpose()?
        .unwrap_or(OutputFormat::Avif);
    let fit = payload.fit.as_deref().map(ImageFit::parse).transpose()?;
    let crop = payload.crop.as_deref().map(ImageCrop::parse).transpose()?;
    let resize = normalize_resize(payload.width, payload.height, fit, crop)?;

    Ok(ParsedPath {
        encoded_payload: encoded_payload.to_string(),
        source: payload.source,
        format,
        width: resize.width,
        height: resize.height,
        fit: resize.fit,
        crop: resize.crop,
        quality: payload.quality.unwrap_or(DEFAULT_QUALITY),
        visibility,
        expires_at_unix_secs,
        private_browser_cache_max_age,
        signature: signature.to_string(),
    })
}

fn parse_public_query(query: &str) -> Result<PublicQuery, ImageError> {
    let mut source = None;
    let mut width = None;
    let mut quality = None;
    let mut format = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "src" if source.is_none() => source = Some(value.into_owned()),
            "w" if width.is_none() => width = Some(value.into_owned()),
            "q" if quality.is_none() => quality = Some(value.into_owned()),
            "f" if format.is_none() => format = Some(value.into_owned()),
            "src" | "w" | "q" | "f" => return Err(ImageError::InvalidUrl),
            _ => return Err(ImageError::InvalidUrl),
        }
    }
    Ok(PublicQuery {
        source: source.ok_or(ImageError::InvalidSource)?,
        width: width.ok_or(ImageError::InvalidWidth)?,
        quality,
        format,
    })
}

fn parse_public_width(value: &str, config: &ImagesConfig) -> Result<u32, ImageError> {
    let width = parse_strict_u32(value).ok_or(ImageError::InvalidWidth)?;
    if config.sizes.contains(&width) {
        Ok(width)
    } else {
        Err(ImageError::InvalidWidth)
    }
}

fn parse_public_quality(value: &str, config: &ImagesConfig) -> Result<u8, ImageError> {
    let quality = parse_strict_u8(value).ok_or(ImageError::InvalidQuality)?;
    if config.qualities.contains(&quality) {
        Ok(quality)
    } else {
        Err(ImageError::InvalidQuality)
    }
}

fn parse_public_format(value: &str, config: &ImagesConfig) -> Result<OutputFormat, ImageError> {
    let format = match value {
        "avif" => OutputFormat::Avif,
        "webp" => OutputFormat::Webp,
        _ => return Err(ImageError::UnsupportedFormat),
    };
    if config.formats.contains(&format) {
        Ok(format)
    } else {
        Err(ImageError::UnsupportedFormat)
    }
}

fn negotiate_public_format(
    accept: Option<&str>,
    config: &ImagesConfig,
) -> Result<OutputFormat, ImageError> {
    if let Some(accept) = accept {
        if accept_contains(accept, "image/avif") && config.formats.contains(&OutputFormat::Avif) {
            return Ok(OutputFormat::Avif);
        }
        if accept_contains(accept, "image/webp") && config.formats.contains(&OutputFormat::Webp) {
            return Ok(OutputFormat::Webp);
        }
    }
    config
        .formats
        .first()
        .copied()
        .ok_or(ImageError::UnsupportedFormat)
}

fn accept_contains(accept: &str, media_type: &str) -> bool {
    accept
        .split(',')
        .filter_map(|part| part.split(';').next())
        .any(|part| part.trim().eq_ignore_ascii_case(media_type))
}

fn parse_strict_u32(value: &str) -> Option<u32> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    value.parse().ok()
}

fn parse_strict_u8(value: &str) -> Option<u8> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    value.parse().ok()
}

fn validate_public_source_allowed(
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

fn validate_pattern_list(patterns: &[String], local: bool) -> Result<(), ImageError> {
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

fn encode_payload(payload: &ImagePayload) -> Result<String, ImageError> {
    let bytes = serde_json::to_vec(payload).map_err(|_| ImageError::InvalidUrl)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_payload(encoded_payload: &str) -> Result<ImagePayload, ImageError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded_payload)
        .map_err(|_| ImageError::InvalidUrl)?;
    serde_json::from_slice(&bytes).map_err(|_| ImageError::InvalidUrl)
}

fn validate_width(width: u32) -> Result<(), ImageError> {
    if ALLOWED_DIMENSIONS.contains(&width) {
        Ok(())
    } else {
        Err(ImageError::InvalidWidth)
    }
}

fn validate_height(height: u32) -> Result<(), ImageError> {
    if ALLOWED_DIMENSIONS.contains(&height) {
        Ok(())
    } else {
        Err(ImageError::InvalidHeight)
    }
}

fn validate_quality(quality: u8) -> Result<(), ImageError> {
    if (1..=100).contains(&quality) {
        Ok(())
    } else {
        Err(ImageError::InvalidQuality)
    }
}

fn normalize_resize(
    width: Option<u32>,
    height: Option<u32>,
    fit: Option<ImageFit>,
    crop: Option<ImageCrop>,
) -> Result<NormalizedResize, ImageError> {
    let width = match width {
        Some(width) => {
            validate_width(width)?;
            width
        }
        None if height.is_some() => return Err(ImageError::InvalidResize),
        None => DEFAULT_WIDTH,
    };
    let Some(height) = height else {
        if fit.is_some() || crop.is_some() {
            return Err(ImageError::InvalidResize);
        }
        return Ok(NormalizedResize {
            width,
            height: None,
            fit: None,
            crop: None,
        });
    };
    validate_height(height)?;

    let fit = fit.unwrap_or(ImageFit::Cover);
    let crop = match fit {
        ImageFit::Cover => Some(crop.unwrap_or(ImageCrop::Center)),
        ImageFit::Contain => {
            if crop.is_some() {
                return Err(ImageError::InvalidResize);
            }
            None
        }
    };

    Ok(NormalizedResize {
        width,
        height: Some(height),
        fit: Some(fit),
        crop,
    })
}

fn browser_cache_duration_from_payload_seconds(seconds: u64) -> Result<Duration, ImageError> {
    let duration = Duration::from_secs(seconds);
    validate_private_browser_cache_max_age(duration)?;
    Ok(duration)
}

fn validate_private_browser_cache_max_age(max_age: Duration) -> Result<(), ImageError> {
    if max_age.is_zero()
        || max_age.as_secs() > MAX_PRIVATE_BROWSER_CACHE_MAX_AGE.as_secs()
        || max_age.subsec_nanos() != 0
    {
        Err(ImageError::InvalidBrowserCacheMaxAge)
    } else {
        Ok(())
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

fn signature(secret: &str, encoded_payload: &str) -> Result<String, ImageError> {
    if secret.is_empty() {
        return Err(ImageError::InvalidSignature);
    }
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| ImageError::InvalidSignature)?;
    mac.update(b"v1\n");
    mac.update(encoded_payload.as_bytes());
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

fn validate_source_format(source: &[u8]) -> Result<(), ImageError> {
    if source.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Ok(());
    }
    if source.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok(());
    }
    if source.len() >= 12 && &source[0..4] == b"RIFF" && &source[8..12] == b"WEBP" {
        return Ok(());
    }
    if source.len() >= 12
        && &source[4..8] == b"ftyp"
        && source[8..].windows(4).any(|brand| brand == b"avif")
    {
        return Ok(());
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

fn thumbnail_image(
    source: &[u8],
    width: u32,
    resize: NormalizedResize,
) -> Result<VipsImage, ImageError> {
    let image = autorotate_image(&read_image_header(source)?)?;
    let source_width = dimension_from_vips(image.get_width())?;
    let source_height = dimension_from_vips(image.get_height())?;
    let scale = resize_scale(source_width, source_height, width, resize)?;
    let resized = if scale < 1.0 {
        resize_image(&image, scale)?
    } else {
        image
    };

    let Some(height) = resize.height else {
        return Ok(resized);
    };
    if resize.fit == Some(ImageFit::Contain) {
        return Ok(resized);
    }

    let resized_width = dimension_from_vips(resized.get_width())?;
    let resized_height = dimension_from_vips(resized.get_height())?;
    let crop_width = width.min(resized_width);
    let crop_height = height.min(resized_height);
    if crop_width == resized_width && crop_height == resized_height {
        return Ok(resized);
    }

    match resize.crop.unwrap_or(ImageCrop::Center) {
        ImageCrop::Center => center_crop_image(&resized, crop_width, crop_height),
        ImageCrop::Smart => smart_crop_image(&resized, crop_width, crop_height),
    }
}

fn resize_scale(
    source_width: u32,
    source_height: u32,
    width: u32,
    resize: NormalizedResize,
) -> Result<f64, ImageError> {
    let width_scale = f64::from(width) / f64::from(source_width);
    let scale = match resize.height {
        None => width_scale,
        Some(height) => {
            let height_scale = f64::from(height) / f64::from(source_height);
            match resize.fit.unwrap_or(ImageFit::Cover) {
                ImageFit::Cover => width_scale.max(height_scale),
                ImageFit::Contain => width_scale.min(height_scale),
            }
        }
    };
    if scale.is_finite() && scale > 0.0 {
        // Requested dimensions are upper bounds. Never enlarge a source image.
        Ok(scale.min(1.0))
    } else {
        Err(ImageError::TransformFailed)
    }
}

fn resize_image(image: &VipsImage, scale: f64) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    ops::resize(image, scale).map_err(|_| vips_transform_error(app))
}

fn center_crop_image(image: &VipsImage, width: u32, height: u32) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    let image_width = dimension_from_vips(image.get_width())?;
    let image_height = dimension_from_vips(image.get_height())?;
    let left = (image_width.saturating_sub(width)) / 2;
    let top = (image_height.saturating_sub(height)) / 2;
    ops::extract_area(
        image,
        i32::try_from(left).map_err(|_| ImageError::TransformFailed)?,
        i32::try_from(top).map_err(|_| ImageError::TransformFailed)?,
        i32::try_from(width).map_err(|_| ImageError::TransformFailed)?,
        i32::try_from(height).map_err(|_| ImageError::TransformFailed)?,
    )
    .map_err(|_| vips_transform_error(app))
}

fn smart_crop_image(image: &VipsImage, width: u32, height: u32) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    ops::smartcrop(
        image,
        i32::try_from(width).map_err(|_| ImageError::TransformFailed)?,
        i32::try_from(height).map_err(|_| ImageError::TransformFailed)?,
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
        OutputFormat::Avif => {
            let suffix = format!(".avif[Q={quality},compression=av1,{STRIP_SOURCE_METADATA}]");
            image
                .image_write_to_buffer(&suffix)
                .map_err(|_| vips_transform_error(app))
        }
        OutputFormat::Webp => {
            let suffix = format!(
                ".webp[Q={quality},alpha-q={quality},smart-subsample=true,preset=photo,{STRIP_SOURCE_METADATA}]"
            );
            image
                .image_write_to_buffer(&suffix)
                .map_err(|_| vips_transform_error(app))
        }
    }
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
