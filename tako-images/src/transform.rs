use crate::{
    ImageCrop, ImageError, ImageFit, NormalizedResize, OutputFormat, normalize_resize,
    validate_quality, validate_width,
};
use libvips::{VipsApp, VipsImage, ops};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

// Some libvips codec paths are process-global even when libvips worker
// concurrency is set to one. Tako gets parallelism from the server worker pool,
// so keep each process to one active transform.
const MAX_PARALLEL_TRANSFORMS: usize = 1;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceFormat {
    Avif,
    Gif,
    Jpeg,
    Png,
    Webp,
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
    let _permit = acquire_vips_transform_permit();
    let source_image = read_image(source, source_format)?;
    let source_width = dimension_from_vips(source_image.get_width())?;
    let source_height = frame_height_from_vips(&source_image)?;
    enforce_dimension_limits(&source_image, source_width, source_height, limits)?;

    if source_width == 0 || source_height == 0 {
        return Err(ImageError::TransformFailed);
    }

    let oriented_image = autorotate_image(&source_image)?;
    let oriented_width = dimension_from_vips(oriented_image.get_width())?;
    let oriented_height = frame_height_from_vips(&oriented_image)?;
    let scale = resize_scale(oriented_width, oriented_height, options.width, resize)?;

    let resized_image;
    let resized = if scale < 1.0 {
        resized_image = resize_image(&oriented_image, scale)?;
        &resized_image
    } else {
        &oriented_image
    };

    let cropped_image;
    let (output, output_height) = if should_crop_image(resized, resize)? {
        let crop_width = resize.width.min(dimension_from_vips(resized.get_width())?);
        let crop_height = resize
            .height
            .ok_or(ImageError::TransformFailed)?
            .min(frame_height_from_vips(resized)?);
        cropped_image = crop_image(
            resized,
            crop_width,
            crop_height,
            resize.crop.unwrap_or(ImageCrop::Center),
        )?;
        (&cropped_image, crop_height)
    } else {
        (resized, frame_height_from_vips(resized)?)
    };

    let width = dimension_from_vips(output.get_width())?;
    let height = output_height;
    let output_format = output_format_for_image(output, options.format)?;
    let bytes = encode_image(output, output_format, options.quality, height)?;

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

fn enforce_dimension_limits(
    image: &VipsImage,
    width: u32,
    height: u32,
    limits: &TransformLimits,
) -> Result<(), ImageError> {
    if width > limits.max_image_width || height > limits.max_image_height {
        return Err(ImageError::ImageTooLarge);
    }
    let pixels = u64::from(width) * u64::from(dimension_from_vips(image.get_height())?);
    if pixels > limits.max_decoded_pixels {
        return Err(ImageError::ImageTooLarge);
    }
    Ok(())
}

fn read_image(source: &[u8], source_format: SourceFormat) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    match source_format {
        SourceFormat::Avif => VipsImage::new_from_buffer(source, ""),
        SourceFormat::Gif => VipsImage::new_from_buffer(source, "[n=-1]"),
        SourceFormat::Webp => VipsImage::new_from_buffer(source, "[n=-1]"),
        SourceFormat::Jpeg | SourceFormat::Png => VipsImage::new_from_buffer(source, ""),
    }
    .map_err(|_| vips_transform_error(app))
}

fn autorotate_image(image: &VipsImage) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    ops::autorot(image).map_err(|_| vips_transform_error(app))
}

fn should_crop_image(image: &VipsImage, resize: NormalizedResize) -> Result<bool, ImageError> {
    let source_width = dimension_from_vips(image.get_width())?;
    let source_height = frame_height_from_vips(image)?;
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

fn output_format_for_image(
    image: &VipsImage,
    requested: OutputFormat,
) -> Result<OutputFormat, ImageError> {
    if requested == OutputFormat::Avif && is_animated_image(image)? {
        return Ok(OutputFormat::Webp);
    }

    Ok(requested)
}

fn crop_image(
    image: &VipsImage,
    width: u32,
    height: u32,
    crop: ImageCrop,
) -> Result<VipsImage, ImageError> {
    if is_animated_image(image)? {
        return crop_animated_image(image, width, height, crop);
    }

    match crop {
        ImageCrop::Center => center_crop_image(image, width, height),
        ImageCrop::Smart => smart_crop_image(image, width, height),
    }
}

fn crop_animated_image(
    image: &VipsImage,
    width: u32,
    height: u32,
    crop: ImageCrop,
) -> Result<VipsImage, ImageError> {
    let pages = n_pages_from_vips(image)?;
    let frame_height = frame_height_from_vips(image)?;
    let total_height = dimension_from_vips(image.get_height())?;
    if u64::from(frame_height) * u64::from(pages) != u64::from(total_height) {
        return Err(ImageError::TransformFailed);
    }

    let mut frames =
        Vec::with_capacity(usize::try_from(pages).map_err(|_| ImageError::TransformFailed)?);
    for page in 0..pages {
        let frame = extract_animation_frame(image, page, frame_height)?;
        let cropped = match crop {
            ImageCrop::Center => center_crop_image(&frame, width, height)?,
            ImageCrop::Smart => smart_crop_image(&frame, width, height)?,
        };
        frames.push(cropped);
    }

    join_animation_frames(frames)
}

fn extract_animation_frame(
    image: &VipsImage,
    page: u32,
    frame_height: u32,
) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    let top = page
        .checked_mul(frame_height)
        .ok_or(ImageError::TransformFailed)?;
    ops::extract_area(
        image,
        0,
        i32::try_from(top).map_err(|_| ImageError::TransformFailed)?,
        image.get_width(),
        i32::try_from(frame_height).map_err(|_| ImageError::TransformFailed)?,
    )
    .map_err(|_| vips_transform_error(app))
}

fn join_animation_frames(frames: Vec<VipsImage>) -> Result<VipsImage, ImageError> {
    let mut frames = frames.into_iter();
    let Some(mut output) = frames.next() else {
        return Err(ImageError::TransformFailed);
    };

    for frame in frames {
        let app = vips_app()?;
        app.error_clear();
        output = ops::join(&output, &frame, ops::Direction::Vertical)
            .map_err(|_| vips_transform_error(app))?;
    }

    Ok(output)
}

fn center_crop_image(image: &VipsImage, width: u32, height: u32) -> Result<VipsImage, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    let image_width = dimension_from_vips(image.get_width())?;
    let image_height = frame_height_from_vips(image)?;
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
    frame_height: u32,
) -> Result<Vec<u8>, ImageError> {
    let app = vips_app()?;
    app.error_clear();
    let page_height = page_height_for_save(image, frame_height)?;
    match format {
        OutputFormat::Avif => {
            if page_height > 0 {
                return Err(ImageError::UnsupportedFormat);
            }
            let suffix = save_suffix_for_avif(quality);
            let bytes = image
                .image_write_to_buffer(&suffix)
                .map_err(|_| vips_transform_error(app))?;
            Ok(copy_vips_allocated_buffer(bytes))
        }
        OutputFormat::Webp => {
            let suffix = save_suffix_for_webp(quality, page_height);
            let bytes = image
                .image_write_to_buffer(&suffix)
                .map_err(|_| vips_transform_error(app))?;
            Ok(copy_vips_allocated_buffer(bytes))
        }
    }
}

fn save_suffix_for_avif(quality: u8) -> String {
    format!(".avif[Q={quality},compression=av1,keep=none]")
}

fn save_suffix_for_webp(quality: u8, page_height: i32) -> String {
    format!(
        ".webp[Q={quality},alpha-q={quality},smart-subsample=true,preset=photo,keep=none,page-height={page_height}]"
    )
}

fn frame_height_from_vips(image: &VipsImage) -> Result<u32, ImageError> {
    let total_height = dimension_from_vips(image.get_height())?;
    let page_height = dimension_from_vips(image.get_page_height())?;
    let pages = n_pages_from_vips(image)?;
    if pages > 1 && total_height % pages == 0 {
        if page_height > 0 && u64::from(page_height) * u64::from(pages) == u64::from(total_height) {
            return Ok(page_height);
        }
        return Ok(total_height / pages);
    }

    if page_height > 0 && page_height <= total_height && total_height % page_height == 0 {
        return Ok(page_height);
    }

    Ok(total_height)
}

fn page_height_for_save(image: &VipsImage, frame_height: u32) -> Result<i32, ImageError> {
    let total_height = dimension_from_vips(image.get_height())?;
    if frame_height > 0 && total_height > frame_height && total_height % frame_height == 0 {
        return i32::try_from(frame_height).map_err(|_| ImageError::TransformFailed);
    }

    Ok(0)
}

fn is_animated_image(image: &VipsImage) -> Result<bool, ImageError> {
    Ok(n_pages_from_vips(image)? > 1)
}

fn n_pages_from_vips(image: &VipsImage) -> Result<u32, ImageError> {
    match image.get_n_pages() {
        pages if pages <= 0 => Ok(1),
        pages => u32::try_from(pages).map_err(|_| ImageError::TransformFailed),
    }
}

fn copy_vips_allocated_buffer(bytes: Vec<u8>) -> Vec<u8> {
    VipsAllocatedBuffer::new(bytes).to_vec()
}

struct VipsAllocatedBuffer {
    bytes: ManuallyDrop<Vec<u8>>,
}

impl VipsAllocatedBuffer {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: ManuallyDrop::new(bytes),
        }
    }

    fn to_vec(&self) -> Vec<u8> {
        self.bytes.as_slice().to_vec()
    }
}

impl Drop for VipsAllocatedBuffer {
    fn drop(&mut self) {
        if self.bytes.capacity() == 0 {
            return;
        }

        // SAFETY: libvips allocates image_write_to_buffer output with GLib. The
        // libvips crate wraps that pointer in Vec, so we suppress Vec's Drop
        // and return ownership to GLib instead of Rust's global allocator.
        unsafe {
            libvips::bindings::g_free(self.bytes.as_mut_ptr().cast::<c_void>());
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
