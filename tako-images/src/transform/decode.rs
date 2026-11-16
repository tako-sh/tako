use super::{
    Decoded, Layout, Raster, SourceFormat, TransformLimits, avif, color, enforce_dimension_limits,
    webp,
};
use crate::ImageError;
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::metadata::Orientation;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, Limits};
use std::io::Cursor;

pub(super) fn decode(
    source: &[u8],
    format: SourceFormat,
    limits: &TransformLimits,
) -> Result<Decoded, ImageError> {
    match format {
        SourceFormat::Jpeg => {
            let decoder = JpegDecoder::new(Cursor::new(source)).map_err(decode_error)?;
            decode_still(decoder, None, limits)
        }
        SourceFormat::Png => {
            let decoder = PngDecoder::new(Cursor::new(source)).map_err(decode_error)?;
            decode_still(decoder, None, limits)
        }
        SourceFormat::Gif => decode_gif(source, limits),
        SourceFormat::Webp => decode_webp(source, limits),
        SourceFormat::Avif => {
            let (raster, profile) = avif::decode(source, limits)?;
            Ok(Decoded::still(raster).with_color_profile(profile))
        }
    }
}

/// Decodes a still image, applies its orientation and records its embedded
/// color profile. `orientation` overrides the decoder's own EXIF reading when
/// the caller parsed it.
fn decode_still(
    mut decoder: impl ImageDecoder,
    orientation: Option<Orientation>,
    limits: &TransformLimits,
) -> Result<Decoded, ImageError> {
    let (width, height) = decoder.dimensions();
    enforce_dimension_limits(width, height, 1, limits)?;
    decoder
        .set_limits(decoder_limits(limits))
        .map_err(|_| ImageError::ImageTooLarge)?;
    // Unreadable EXIF should not reject an otherwise valid image.
    let orientation = match orientation {
        Some(orientation) => orientation,
        None => decoder.orientation().unwrap_or(Orientation::NoTransforms),
    };
    // Likewise, an unreadable or unusable profile only skips color conversion.
    let profile = decoder
        .icc_profile()
        .ok()
        .flatten()
        .and_then(|icc| color::profile_from_icc(&icc));
    let mut image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    image.apply_orientation(orientation);
    Ok(Decoded::still(raster_from_dynamic(image)?).with_color_profile(profile))
}

fn raster_from_dynamic(image: DynamicImage) -> Result<Raster, ImageError> {
    let (width, height) = (image.width(), image.height());
    if image.color().has_alpha() {
        Raster::new(width, height, Layout::Rgba, image.into_rgba8().into_raw())
    } else {
        Raster::new(width, height, Layout::Rgb, image.into_rgb8().into_raw())
    }
}

fn decode_gif(source: &[u8], limits: &TransformLimits) -> Result<Decoded, ImageError> {
    let mut decoder = GifDecoder::new(Cursor::new(source)).map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    enforce_dimension_limits(width, height, 1, limits)?;
    decoder
        .set_limits(decoder_limits(limits))
        .map_err(|_| ImageError::ImageTooLarge)?;

    let mut frames = Vec::new();
    let mut delays_ms = Vec::new();
    for frame in decoder.into_frames() {
        let frame = frame.map_err(decode_error)?;
        // Enforce the cumulative frame budget before keeping another frame.
        let count = u32::try_from(frames.len() + 1).map_err(|_| ImageError::ImageTooLarge)?;
        enforce_dimension_limits(width, height, count, limits)?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        delays_ms.push(numerator.checked_div(denominator).unwrap_or(0));
        let buffer = frame.into_buffer();
        frames.push(Raster::new(width, height, Layout::Rgba, buffer.into_raw())?);
    }
    if frames.is_empty() {
        return Err(ImageError::TransformFailed);
    }
    if frames.len() == 1 {
        return Ok(Decoded::still(frames.remove(0)));
    }

    Ok(Decoded {
        frames,
        delays_ms,
        loop_count: gif_loop_count(source)?,
        // GIF has no standard color profile; palettes are treated as sRGB.
        color_profile: None,
    })
}

/// Converts the GIF NETSCAPE repeat count into WebP loop semantics. GIF stores
/// the number of extra repetitions, WebP the total number of plays, and a GIF
/// without the extension plays once.
fn gif_loop_count(source: &[u8]) -> Result<u16, ImageError> {
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    let mut decoder = options
        .read_info(Cursor::new(source))
        .map_err(|_| ImageError::TransformFailed)?;
    // The extension may appear anywhere before a frame, so walk the frame headers.
    while decoder
        .next_frame_info()
        .map_err(|_| ImageError::TransformFailed)?
        .is_some()
    {}
    Ok(match decoder.repeat() {
        gif::Repeat::Infinite => 0,
        gif::Repeat::Finite(repeats) => repeats.saturating_add(1),
    })
}

fn decode_webp(source: &[u8], limits: &TransformLimits) -> Result<Decoded, ImageError> {
    let mut decoder = image_webp::WebPDecoder::new(Cursor::new(source))
        .map_err(|_| ImageError::TransformFailed)?;
    let orientation = webp_orientation(&mut decoder);
    if !decoder.is_animated() || decoder.num_frames() <= 1 {
        let decoder = WebPDecoder::new(Cursor::new(source)).map_err(decode_error)?;
        return decode_still(decoder, Some(orientation), limits);
    }

    // Header dimensions and frame count bound the work before decoding.
    let (width, height) = decoder.dimensions();
    let frame_count = decoder.num_frames();
    enforce_dimension_limits(width, height, frame_count, limits)?;

    // libwebp's reference animation decoder composites frames onto an RGBA
    // canvas and clears disposed areas to transparent, as browsers do, even
    // when the VP8X alpha flag is unset (image-webp drops that alpha).
    let mut animation = webp::decode_animation(source, width, height, frame_count)?;
    // Most animations are opaque; RGB lightens resize and encode work.
    let opaque = !animation.frames.iter().any(Raster::has_transparency);
    animation.frames = animation
        .frames
        .into_iter()
        .map(|frame| {
            let frame = if opaque { drop_alpha(frame)? } else { frame };
            // Animated files may carry EXIF too; orient every frame like stills.
            orient_raster(frame, orientation)
        })
        .collect::<Result<_, _>>()?;
    let profile = decoder
        .icc_profile()
        .ok()
        .flatten()
        .and_then(|icc| color::profile_from_icc(&icc));
    Ok(animation.with_color_profile(profile))
}

fn drop_alpha(raster: Raster) -> Result<Raster, ImageError> {
    if raster.layout == Layout::Rgb {
        return Ok(raster);
    }
    let data = raster
        .data
        .chunks_exact(4)
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    Raster::new(raster.width, raster.height, Layout::Rgb, data)
}

/// Reads the EXIF orientation of a still or animated WebP. The WebP spec stores
/// a bare TIFF header, but common writers keep JPEG's `Exif\0\0` prefix, which
/// `Orientation::from_exif_chunk` rejects; libvips accepts both.
fn webp_orientation<R: std::io::BufRead + std::io::Seek>(
    decoder: &mut image_webp::WebPDecoder<R>,
) -> Orientation {
    decoder
        .exif_metadata()
        .ok()
        .flatten()
        .and_then(|exif| {
            let tiff = exif.strip_prefix(b"Exif\0\0").unwrap_or(&exif);
            Orientation::from_exif_chunk(tiff)
        })
        .unwrap_or(Orientation::NoTransforms)
}

fn orient_raster(raster: Raster, orientation: Orientation) -> Result<Raster, ImageError> {
    if orientation == Orientation::NoTransforms {
        return Ok(raster);
    }
    let Raster {
        width,
        height,
        layout,
        data,
    } = raster;
    let mut image = match layout {
        Layout::Rgb => image::RgbImage::from_raw(width, height, data).map(DynamicImage::ImageRgb8),
        Layout::Rgba => {
            image::RgbaImage::from_raw(width, height, data).map(DynamicImage::ImageRgba8)
        }
    }
    .ok_or(ImageError::TransformFailed)?;
    image.apply_orientation(orientation);
    raster_from_dynamic(image)
}

fn decoder_limits(limits: &TransformLimits) -> Limits {
    let mut decoder_limits = Limits::default();
    decoder_limits.max_image_width = Some(limits.max_image_width);
    decoder_limits.max_image_height = Some(limits.max_image_height);
    decoder_limits.max_alloc = Some(memory_limit(limits) as u64);
    decoder_limits
}

/// Allocation ceiling for decoders: 16-bit RGBA for every permitted pixel plus
/// headroom for codec scratch buffers.
fn memory_limit(limits: &TransformLimits) -> usize {
    let pixels = usize::try_from(limits.max_decoded_pixels).unwrap_or(usize::MAX);
    pixels.saturating_mul(8).saturating_add(64 * 1024 * 1024)
}

fn decode_error(error: image::ImageError) -> ImageError {
    match error {
        image::ImageError::Limits(_) => ImageError::ImageTooLarge,
        _ => ImageError::TransformFailed,
    }
}
