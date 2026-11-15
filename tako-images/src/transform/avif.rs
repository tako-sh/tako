//! AVIF decoding through statically linked libavif + dav1d, and AVIF encoding
//! through ravif (rav1e).

use super::{Layout, Raster, TransformLimits, enforce_dimension_limits};
use crate::ImageError;
use libavif_sys as sys;
use rgb::FromSlice;

/// AVIF output effort. rav1e speed 6 keeps still encodes bounded on one core.
const AVIF_ENCODER_SPEED: u8 = 6;

pub(super) fn encode(raster: &Raster, quality: u8) -> Result<Vec<u8>, ImageError> {
    let encoder = ravif::Encoder::new()
        .with_quality(f32::from(quality))
        .with_alpha_quality(f32::from(quality))
        .with_speed(AVIF_ENCODER_SPEED)
        .with_num_threads(Some(1));
    let width = raster.width as usize;
    let height = raster.height as usize;
    let encoded = match raster.layout {
        Layout::Rgba => encoder.encode_rgba(imgref::Img::new(raster.data.as_rgba(), width, height)),
        Layout::Rgb => encoder.encode_rgb(imgref::Img::new(raster.data.as_rgb(), width, height)),
    }
    .map_err(|_| ImageError::TransformFailed)?;
    Ok(encoded.avif_file)
}

/// Decodes the primary (or first) AVIF image and applies its clean aperture,
/// rotation and mirror properties. Metadata is ignored.
pub(super) fn decode(source: &[u8], limits: &TransformLimits) -> Result<Raster, ImageError> {
    let decoder = Decoder::new()?;
    let raster = {
        // SAFETY: `decoder.0` is a live decoder owned by `decoder` for this
        // whole block. `source` outlives every libavif call that reads it,
        // because the decoder is dropped at the end of this function.
        unsafe {
            let raw = decoder.0;
            (*raw).maxThreads = 1;
            // Accept files written by older encoders (for example a missing
            // alpha `ispe`), as other browsers and libheif do.
            (*raw).strictFlags = sys::AVIF_STRICT_DISABLED as sys::avifStrictFlags;
            (*raw).ignoreExif = sys::AVIF_TRUE as sys::avifBool;
            (*raw).ignoreXMP = sys::AVIF_TRUE as sys::avifBool;
            check(sys::avifDecoderSetIOMemory(
                raw,
                source.as_ptr(),
                source.len(),
            ))?;
            check(sys::avifDecoderParse(raw))?;
            let image = (*raw).image;
            if image.is_null() {
                return Err(ImageError::TransformFailed);
            }
            // Parsing reads only headers, so the decoded size is checked
            // before any AV1 payload is decoded.
            enforce_dimension_limits((*image).width, (*image).height, 1, limits)?;
            check(sys::avifDecoderNextImage(raw))?;
            let image = (*raw).image;
            if image.is_null() {
                return Err(ImageError::TransformFailed);
            }
            let raster = image_to_raster(image)?;
            apply_transforms(raster, image, raw)?
        }
    };
    Ok(raster)
}

/// Converts a decoded YUV(A) image to 8-bit interleaved RGB(A).
///
/// # Safety
/// `image` must point at a valid decoded `avifImage`.
unsafe fn image_to_raster(image: *const sys::avifImage) -> Result<Raster, ImageError> {
    let mut rgb = RgbImage(sys::avifRGBImage::default());
    // SAFETY: `image` is valid per this function's contract, and `rgb.0` is a
    // zeroed struct that libavif initializes and later owns pixels for.
    unsafe {
        let has_alpha = !(*image).alphaPlane.is_null();
        sys::avifRGBImageSetDefaults(&mut rgb.0, image);
        rgb.0.depth = 8;
        rgb.0.maxThreads = 1;
        rgb.0.format = if has_alpha {
            sys::AVIF_RGB_FORMAT_RGBA
        } else {
            sys::AVIF_RGB_FORMAT_RGB
        };
        check(sys::avifRGBImageAllocatePixels(&mut rgb.0))?;
        check(sys::avifImageYUVToRGB(image, &mut rgb.0))?;

        let layout = if has_alpha { Layout::Rgba } else { Layout::Rgb };
        let width = rgb.0.width;
        let height = rgb.0.height;
        let row_bytes = width as usize * layout.channels();
        let stride = rgb.0.rowBytes as usize;
        if rgb.0.pixels.is_null() || stride < row_bytes {
            return Err(ImageError::TransformFailed);
        }
        let pixels = std::slice::from_raw_parts(rgb.0.pixels, stride * height as usize);
        let mut data = Vec::with_capacity(row_bytes * height as usize);
        for row in pixels.chunks_exact(stride) {
            data.extend_from_slice(&row[..row_bytes]);
        }
        Raster::new(width, height, layout, data)
    }
}

/// Applies HEIF transformative properties in their mandated order:
/// clean aperture, then rotation, then mirroring.
///
/// # Safety
/// `image` and `decoder` must be valid and belong together.
unsafe fn apply_transforms(
    mut raster: Raster,
    image: *const sys::avifImage,
    decoder: *mut sys::avifDecoder,
) -> Result<Raster, ImageError> {
    // SAFETY: both pointers are valid per this function's contract.
    let (flags, clap, angle, axis) = unsafe {
        (
            (*image).transformFlags,
            (*image).clap,
            (*image).irot.angle,
            (*image).imir.axis,
        )
    };

    if flags & sys::AVIF_TRANSFORM_CLAP != 0 {
        let mut rect = sys::avifCropRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        // SAFETY: `decoder` is valid; libavif writes only into `rect` and diag.
        let valid = unsafe {
            sys::avifCropRectFromCleanApertureBox(
                &mut rect,
                &clap,
                raster.width,
                raster.height,
                &mut (*decoder).diag,
            )
        };
        // Invalid apertures are ignored, matching non-strict decoding.
        if valid != 0
            && rect.width > 0
            && rect.height > 0
            && (rect.width, rect.height) != (raster.width, raster.height)
        {
            raster = super::crop_raster(&raster, rect.x, rect.y, rect.width, rect.height)?;
        }
    }
    if flags & sys::AVIF_TRANSFORM_IROT != 0 {
        // irot angles are anti-clockwise quarter turns.
        raster = match angle % 4 {
            1 => rotate_clockwise(&raster, 3),
            2 => rotate_clockwise(&raster, 2),
            3 => rotate_clockwise(&raster, 1),
            _ => raster,
        };
    }
    if flags & sys::AVIF_TRANSFORM_IMIR != 0 {
        raster = if axis == 0 {
            flip_vertical(&raster)
        } else {
            flip_horizontal(&raster)
        };
    }
    Ok(raster)
}

fn rotate_clockwise(raster: &Raster, quarter_turns: u8) -> Raster {
    let channels = raster.layout.channels();
    let (width, height) = (raster.width as usize, raster.height as usize);
    let (out_width, out_height) = if quarter_turns % 2 == 1 {
        (height, width)
    } else {
        (width, height)
    };
    let mut data = vec![0; raster.data.len()];
    for y in 0..height {
        for x in 0..width {
            let (out_x, out_y) = match quarter_turns % 4 {
                1 => (height - 1 - y, x),
                2 => (width - 1 - x, height - 1 - y),
                3 => (y, width - 1 - x),
                _ => (x, y),
            };
            let source = (y * width + x) * channels;
            let target = (out_y * out_width + out_x) * channels;
            data[target..target + channels]
                .copy_from_slice(&raster.data[source..source + channels]);
        }
    }
    Raster {
        width: out_width as u32,
        height: out_height as u32,
        layout: raster.layout,
        data,
    }
}

fn flip_horizontal(raster: &Raster) -> Raster {
    let channels = raster.layout.channels();
    let mut data = Vec::with_capacity(raster.data.len());
    for row in raster.data.chunks_exact(raster.row_bytes()) {
        for pixel in row.chunks_exact(channels).rev() {
            data.extend_from_slice(pixel);
        }
    }
    Raster {
        data,
        ..raster.clone()
    }
}

fn flip_vertical(raster: &Raster) -> Raster {
    let mut data = Vec::with_capacity(raster.data.len());
    for row in raster.data.chunks_exact(raster.row_bytes()).rev() {
        data.extend_from_slice(row);
    }
    Raster {
        data,
        ..raster.clone()
    }
}

fn check(result: sys::avifResult) -> Result<(), ImageError> {
    if result == sys::AVIF_RESULT_OK {
        Ok(())
    } else {
        Err(ImageError::TransformFailed)
    }
}

struct Decoder(*mut sys::avifDecoder);

impl Decoder {
    fn new() -> Result<Self, ImageError> {
        // SAFETY: avifDecoderCreate has no preconditions; null means OOM.
        let raw = unsafe { sys::avifDecoderCreate() };
        if raw.is_null() {
            Err(ImageError::TransformFailed)
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from avifDecoderCreate and is destroyed once.
        unsafe { sys::avifDecoderDestroy(self.0) }
    }
}

struct RgbImage(sys::avifRGBImage);

impl Drop for RgbImage {
    fn drop(&mut self) {
        // SAFETY: freeing is a no-op for unallocated (null) pixels.
        unsafe { sys::avifRGBImageFreePixels(&mut self.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(width: u32, height: u32) -> Raster {
        let data = (0..width * height).flat_map(|i| [i as u8, 0, 0]).collect();
        Raster::new(width, height, Layout::Rgb, data).expect("raster")
    }

    fn red(raster: &Raster) -> Vec<u8> {
        raster.data.chunks_exact(3).map(|pixel| pixel[0]).collect()
    }

    #[test]
    fn rotates_quarter_turns_clockwise() {
        // 0 1 2
        // 3 4 5
        let source = numbered(3, 2);
        let once = rotate_clockwise(&source, 1);
        assert_eq!((once.width, once.height), (2, 3));
        assert_eq!(red(&once), [3, 0, 4, 1, 5, 2]);
        let half = rotate_clockwise(&source, 2);
        assert_eq!(red(&half), [5, 4, 3, 2, 1, 0]);
        let thrice = rotate_clockwise(&source, 3);
        assert_eq!(red(&thrice), [2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn mirrors_rows_and_columns() {
        let source = numbered(3, 2);
        assert_eq!(red(&flip_horizontal(&source)), [2, 1, 0, 5, 4, 3]);
        assert_eq!(red(&flip_vertical(&source)), [3, 4, 5, 0, 1, 2]);
    }
}
