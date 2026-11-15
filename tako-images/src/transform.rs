use crate::{
    ImageCrop, ImageError, ImageFit, NormalizedResize, OutputFormat, normalize_resize,
    validate_quality, validate_width,
};
use fast_image_resize as fr;

mod avif;
mod decode;
mod smartcrop;
mod webp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformLimits {
    pub max_source_bytes: usize,
    pub max_image_width: u32,
    pub max_image_height: u32,
    /// Upper bound on decoded pixels across all frames of the source.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceFormat {
    Avif,
    Gif,
    Jpeg,
    Png,
    Webp,
}

/// Interleaved 8-bit sRGB pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Rgb,
    Rgba,
}

impl Layout {
    fn channels(self) -> usize {
        match self {
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }

    fn pixel_type(self) -> fr::PixelType {
        match self {
            Self::Rgb => fr::PixelType::U8x3,
            Self::Rgba => fr::PixelType::U8x4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Raster {
    width: u32,
    height: u32,
    layout: Layout,
    data: Vec<u8>,
}

impl Raster {
    fn new(width: u32, height: u32, layout: Layout, data: Vec<u8>) -> Result<Self, ImageError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(layout.channels()))
            .ok_or(ImageError::TransformFailed)?;
        if width == 0 || height == 0 || data.len() != expected {
            return Err(ImageError::TransformFailed);
        }
        Ok(Self {
            width,
            height,
            layout,
            data,
        })
    }

    fn row_bytes(&self) -> usize {
        self.width as usize * self.layout.channels()
    }

    fn has_transparency(&self) -> bool {
        self.layout == Layout::Rgba && self.data.chunks_exact(4).any(|pixel| pixel[3] != 255)
    }
}

/// A decoded source: one frame for still images, several for animations.
/// Every frame has the same dimensions and layout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Decoded {
    frames: Vec<Raster>,
    /// Display duration of each frame in milliseconds. Empty for still images.
    delays_ms: Vec<u32>,
    /// WebP loop semantics: 0 loops forever, otherwise the total number of plays.
    loop_count: u16,
}

impl Decoded {
    fn still(raster: Raster) -> Self {
        Self {
            frames: vec![raster],
            delays_ms: Vec::new(),
            loop_count: 0,
        }
    }

    fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    fn first(&self) -> Result<&Raster, ImageError> {
        self.frames.first().ok_or(ImageError::TransformFailed)
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

    let source_format = detect_source_format(source)?;
    // Decoding applies EXIF / irot / imir orientation to the pixels.
    let mut image = decode::decode(source, source_format, limits)?;

    let first = image.first()?;
    let (oriented_width, oriented_height) = (first.width, first.height);
    let scale = resize_scale(oriented_width, oriented_height, options.width, resize)?;
    if scale < 1.0 {
        let width = scaled_dimension(oriented_width, scale);
        let height = scaled_dimension(oriented_height, scale);
        let mut resizer = fr::Resizer::new();
        for frame in &mut image.frames {
            *frame = resize_raster(&mut resizer, frame, width, height)?;
        }
    }

    if let Some((crop_width, crop_height)) = crop_size(image.first()?, resize) {
        let first = image.first()?;
        // One crop window for every frame keeps animations steady.
        let (left, top) = match resize.crop.unwrap_or(ImageCrop::Center) {
            ImageCrop::Center => center_crop_offset(first, crop_width, crop_height),
            ImageCrop::Smart => smartcrop::attention_offset(first, crop_width, crop_height),
        };
        for frame in &mut image.frames {
            *frame = crop_raster(frame, left, top, crop_width, crop_height)?;
        }
    }

    let output = image.first()?;
    let (width, height) = (output.width, output.height);
    let output_format = output_format_for_image(&image, options.format);
    let bytes = match output_format {
        OutputFormat::Avif => avif::encode(output, options.quality)?,
        OutputFormat::Webp if image.is_animated() => {
            webp::encode_animation(&image, options.quality)?
        }
        OutputFormat::Webp => webp::encode_still(output, options.quality)?,
    };

    Ok(TransformedImage {
        bytes,
        content_type: output_format.content_type(),
        format: output_format,
        width,
        height,
    })
}

fn detect_source_format(source: &[u8]) -> Result<SourceFormat, ImageError> {
    if source.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Ok(SourceFormat::Png);
    }
    if source.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok(SourceFormat::Jpeg);
    }
    if source.starts_with(b"GIF87a") || source.starts_with(b"GIF89a") {
        return Ok(SourceFormat::Gif);
    }
    if source.len() >= 12 && &source[0..4] == b"RIFF" && &source[8..12] == b"WEBP" {
        return Ok(SourceFormat::Webp);
    }
    if source.len() >= 12
        && &source[4..8] == b"ftyp"
        && source[8..]
            .windows(4)
            .any(|brand| brand == b"avif" || brand == b"avis")
    {
        return Ok(SourceFormat::Avif);
    }

    Err(ImageError::UnsupportedFormat)
}

/// Rejects sources whose decoded size exceeds the configured limits. Called
/// with header dimensions before any pixel data is decoded.
fn enforce_dimension_limits(
    width: u32,
    height: u32,
    frames: u32,
    limits: &TransformLimits,
) -> Result<(), ImageError> {
    if width == 0 || height == 0 || frames == 0 {
        return Err(ImageError::TransformFailed);
    }
    if width > limits.max_image_width || height > limits.max_image_height {
        return Err(ImageError::ImageTooLarge);
    }
    let pixels = u64::from(width) * u64::from(height) * u64::from(frames);
    if pixels > limits.max_decoded_pixels {
        return Err(ImageError::ImageTooLarge);
    }
    Ok(())
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

fn scaled_dimension(dimension: u32, scale: f64) -> u32 {
    // Matches libvips' resize rounding so output sizes stay stable.
    let scaled = (f64::from(dimension) * scale).round();
    if scaled < 1.0 { 1 } else { scaled as u32 }
}

fn resize_raster(
    resizer: &mut fr::Resizer,
    raster: &Raster,
    width: u32,
    height: u32,
) -> Result<Raster, ImageError> {
    let pixel_type = raster.layout.pixel_type();
    let source = fr::images::ImageRef::new(raster.width, raster.height, &raster.data, pixel_type)
        .map_err(|_| ImageError::TransformFailed)?;
    let mut destination = fr::images::Image::new(width, height, pixel_type);
    // Lanczos3 matches the libvips resize kernel. RGBA is premultiplied while
    // filtering so transparent pixels do not bleed color into visible edges.
    let options = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3))
        .use_alpha(true);
    resizer
        .resize(&source, &mut destination, &options)
        .map_err(|_| ImageError::TransformFailed)?;
    Raster::new(width, height, raster.layout, destination.into_vec())
}

/// Returns the crop box for cover resizes whose scaled image overflows the
/// requested box. Contain and width-only resizes never crop.
fn crop_size(image: &Raster, resize: NormalizedResize) -> Option<(u32, u32)> {
    let height = resize.height?;
    if resize.fit == Some(ImageFit::Contain) {
        return None;
    }
    let crop_width = resize.width.min(image.width);
    let crop_height = height.min(image.height);
    (crop_width != image.width || crop_height != image.height).then_some((crop_width, crop_height))
}

fn center_crop_offset(image: &Raster, width: u32, height: u32) -> (u32, u32) {
    (
        image.width.saturating_sub(width) / 2,
        image.height.saturating_sub(height) / 2,
    )
}

fn crop_raster(
    raster: &Raster,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
) -> Result<Raster, ImageError> {
    if left
        .checked_add(width)
        .is_none_or(|right| right > raster.width)
        || top
            .checked_add(height)
            .is_none_or(|bottom| bottom > raster.height)
    {
        return Err(ImageError::TransformFailed);
    }
    let channels = raster.layout.channels();
    let source_stride = raster.row_bytes();
    let row_bytes = width as usize * channels;
    let mut data = Vec::with_capacity(row_bytes * height as usize);
    for row in top..top + height {
        let start = row as usize * source_stride + left as usize * channels;
        data.extend_from_slice(&raster.data[start..start + row_bytes]);
    }
    Raster::new(width, height, raster.layout, data)
}

/// Decodes an animated WebP with libwebp's reference decoder for tests:
/// returns each composited RGBA frame and its duration in milliseconds.
#[cfg(test)]
pub(crate) fn decode_webp_animation_frames(
    bytes: &[u8],
) -> Result<(Vec<Vec<u8>>, Vec<u32>), ImageError> {
    let decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|_| ImageError::TransformFailed)?;
    let (width, height) = decoder.dimensions();
    let decoded = webp::decode_animation(bytes, width, height, decoder.num_frames())?;
    let frames = decoded.frames.into_iter().map(|frame| frame.data).collect();
    Ok((frames, decoded.delays_ms))
}

fn output_format_for_image(image: &Decoded, requested: OutputFormat) -> OutputFormat {
    // Browser-timed AVIF animation is not produced; animated sources keep
    // their animation as WebP instead.
    if requested == OutputFormat::Avif && image.is_animated() {
        return OutputFormat::Webp;
    }
    requested
}
