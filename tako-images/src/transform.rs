use crate::{
    ImageCrop, ImageError, ImageFit, NormalizedResize, OutputFormat, normalize_resize,
    validate_quality, validate_width,
};
use libvips::{VipsApp, VipsImage, ops};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

const STRIP_SOURCE_METADATA: &str = "strip";
const MAX_PARALLEL_TRANSFORMS: usize = 2;

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
    let _permit = acquire_vips_transform_permit();
    let source_image = read_image(source)?;
    let source_width = dimension_from_vips(source_image.get_width())?;
    let source_height = dimension_from_vips(source_image.get_height())?;
    enforce_dimension_limits(source_width, source_height, limits)?;

    if source_width == 0 || source_height == 0 {
        return Err(ImageError::TransformFailed);
    }

    let oriented_image = autorotate_image(&source_image)?;
    let oriented_width = dimension_from_vips(oriented_image.get_width())?;
    let oriented_height = dimension_from_vips(oriented_image.get_height())?;
    let scale = resize_scale(oriented_width, oriented_height, options.width, resize)?;

    let resized_image;
    let resized = if scale < 1.0 {
        resized_image = resize_image(&oriented_image, scale)?;
        &resized_image
    } else {
        &oriented_image
    };

    let cropped_image;
    let output = if should_crop_image(resized, resize)? {
        let crop_width = resize.width.min(dimension_from_vips(resized.get_width())?);
        let crop_height = resize
            .height
            .ok_or(ImageError::TransformFailed)?
            .min(dimension_from_vips(resized.get_height())?);
        cropped_image = match resize.crop.unwrap_or(ImageCrop::Center) {
            ImageCrop::Center => center_crop_image(resized, crop_width, crop_height)?,
            ImageCrop::Smart => smart_crop_image(resized, crop_width, crop_height)?,
        };
        &cropped_image
    } else {
        resized
    };

    let width = dimension_from_vips(output.get_width())?;
    let height = dimension_from_vips(output.get_height())?;
    let bytes = encode_image(output, options.format, options.quality)?;

    Ok(TransformedImage {
        bytes,
        content_type: options.format.content_type(),
        format: options.format,
        width,
        height,
    })
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

fn read_image(source: &[u8]) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    VipsImage::new_from_buffer(source, "").map_err(|_| vips_transform_error(app))
}

fn autorotate_image(image: &VipsImage) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    ops::autorot(image).map_err(|_| vips_transform_error(app))
}

fn should_crop_image(image: &VipsImage, resize: NormalizedResize) -> Result<bool, ImageError> {
    let source_width = dimension_from_vips(image.get_width())?;
    let source_height = dimension_from_vips(image.get_height())?;
    let Some(height) = resize.height else {
        return Ok(false);
    };
    if resize.fit == Some(ImageFit::Contain) {
        return Ok(false);
    }

    Ok(
        resize.width.min(source_width) != source_width
            || height.min(source_height) != source_height,
    )
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
        app.concurrency_set(1);
        app.cache_set_max(100);
        app.cache_set_max_mem(128 * 1024 * 1024);
        Ok(app)
    }) {
        Ok(app) => Ok(app),
        Err(()) => Err(ImageError::TransformFailed),
    }
}

fn lock_active_count(mutex: &Mutex<usize>) -> MutexGuard<'_, usize> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn wait_for_transform_slot<'a>(
    available: &Condvar,
    active: MutexGuard<'a, usize>,
) -> MutexGuard<'a, usize> {
    match available.wait(active) {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct TransformGate {
    active: Mutex<usize>,
    available: Condvar,
    capacity: usize,
}

impl TransformGate {
    fn new(capacity: usize) -> Self {
        Self {
            active: Mutex::new(0),
            available: Condvar::new(),
            capacity,
        }
    }

    fn acquire(&'static self) -> TransformPermit {
        let mut active = lock_active_count(&self.active);
        while *active >= self.capacity {
            active = wait_for_transform_slot(&self.available, active);
        }
        *active += 1;
        TransformPermit { gate: self }
    }

    fn release(&self) {
        let mut active = lock_active_count(&self.active);
        if *active > 0 {
            *active -= 1;
            self.available.notify_one();
        }
    }
}

struct TransformPermit {
    gate: &'static TransformGate,
}

impl Drop for TransformPermit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

fn acquire_vips_transform_permit() -> TransformPermit {
    vips_transform_gate().acquire()
}

fn vips_transform_gate() -> &'static TransformGate {
    static GATE: OnceLock<TransformGate> = OnceLock::new();
    GATE.get_or_init(|| TransformGate::new(MAX_PARALLEL_TRANSFORMS))
}

#[cfg(test)]
pub(crate) fn vips_transform_capacity() -> usize {
    vips_transform_gate().capacity
}

#[cfg(test)]
pub(crate) fn occupy_vips_transform_slot() -> impl Drop {
    acquire_vips_transform_permit()
}

#[cfg(test)]
pub(crate) fn vips_concurrency() -> Result<i32, ImageError> {
    Ok(vips_app()?.concurrency_get())
}
