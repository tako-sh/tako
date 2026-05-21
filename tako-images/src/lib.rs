use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::borrow::Cow;
use std::time::Duration;

mod source;
mod transform;

pub use source::{ImageSource, ip_is_private_or_local};
use source::{parse_source, validate_pattern_list, validate_public_source_allowed};
pub use transform::{TransformLimits, TransformOptions, TransformedImage, transform_image};

pub const IMAGE_BASE_PATH: &str = "/_tako/image/v1";
pub const PUBLIC_IMAGE_BASE_PATH: &str = "/_tako/image";
pub const DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE: Duration = Duration::from_secs(604_800);
pub const MAX_PRIVATE_BROWSER_CACHE_MAX_AGE: Duration = Duration::from_secs(31_536_000);
pub const PUBLIC_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
pub const PRIVATE_CACHE_CONTROL: &str = "private, max-age=604800";

const ALLOWED_DIMENSIONS: &[u32] = &[
    16, 32, 48, 64, 96, 128, 256, 320, 384, 640, 750, 828, 960, 1080, 1200, 1920, 2048, 3840,
];
const DEFAULT_WIDTH: u32 = 1200;
const DEFAULT_QUALITY: u8 = 75;
const DEFAULT_PUBLIC_WIDTHS: &[u32] = &[320, 640, 960, 1200, 1920];
const DEFAULT_PUBLIC_QUALITIES: &[u8] = &[75];
const DEFAULT_PUBLIC_FORMATS: &[OutputFormat] = &[OutputFormat::Webp];

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
    pub vary_accept: bool,
    pub width: u32,
    pub height: Option<u32>,
    pub fit: Option<ImageFit>,
    pub crop: Option<ImageCrop>,
    pub quality: u8,
    pub visibility: ImageVisibility,
    pub expires_at_unix_secs: Option<u64>,
    pub private_browser_cache_max_age: Option<Duration>,
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
    #[error("image transform queue is full")]
    TransformQueueFull,
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
        format: (options.format != OutputFormat::Webp)
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
        vary_accept: false,
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
    let vary_accept = params.format.is_none();
    let format = match params.format.as_deref() {
        Some(value) => parse_public_format(value, config)?,
        None => negotiate_public_format(accept, config)?,
    };

    Ok(VerifiedImageRequest {
        source,
        format,
        vary_accept,
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

impl OutputFormat {
    fn parse_payload_override(value: &str) -> Result<Self, ImageError> {
        match value {
            "avif" => Ok(Self::Avif),
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
        .unwrap_or(OutputFormat::Webp);
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
        for format in &config.formats {
            if accept_contains(accept, format.content_type()) {
                return Ok(*format);
            }
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

#[cfg(test)]
mod tests;
