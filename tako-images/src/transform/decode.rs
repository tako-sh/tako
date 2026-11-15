use super::{
    Decoded, Layout, Raster, SourceFormat, TransformLimits, avif, enforce_dimension_limits,
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
            decode_still(decoder, limits).map(Decoded::still)
        }
        SourceFormat::Png => {
            let decoder = PngDecoder::new(Cursor::new(source)).map_err(decode_error)?;
            decode_still(decoder, limits).map(Decoded::still)
        }
        SourceFormat::Gif => decode_gif(source, limits),
        SourceFormat::Webp => decode_webp(source, limits),
        SourceFormat::Avif => avif::decode(source, limits).map(Decoded::still),
    }
}

fn decode_still(
    mut decoder: impl ImageDecoder,
    limits: &TransformLimits,
) -> Result<Raster, ImageError> {
    let (width, height) = decoder.dimensions();
    enforce_dimension_limits(width, height, 1, limits)?;
    decoder
        .set_limits(decoder_limits(limits))
        .map_err(|_| ImageError::ImageTooLarge)?;
    // Unreadable EXIF should not reject an otherwise valid image.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    image.apply_orientation(orientation);
    raster_from_dynamic(image)
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
    if !decoder.is_animated() || decoder.num_frames() <= 1 {
        // The still path applies EXIF orientation like JPEG and PNG.
        let decoder = WebPDecoder::new(Cursor::new(source)).map_err(decode_error)?;
        return decode_still(decoder, limits).map(Decoded::still);
    }

    let (width, height) = decoder.dimensions();
    let frame_count = decoder.num_frames();
    enforce_dimension_limits(width, height, frame_count, limits)?;
    decoder.set_memory_limit(memory_limit(limits));
    // image-webp skips "dispose to background" unless a background is set.
    // Dispose to transparent, as libwebp and browsers do.
    decoder
        .set_background_color([0, 0, 0, 0])
        .map_err(|_| ImageError::TransformFailed)?;
    let layout = if decoder.has_alpha() {
        Layout::Rgba
    } else {
        Layout::Rgb
    };
    let frame_bytes = decoder
        .output_buffer_size()
        .ok_or(ImageError::ImageTooLarge)?;

    let mut frames = Vec::with_capacity(frame_count as usize);
    let mut delays_ms = Vec::with_capacity(frame_count as usize);
    for _ in 0..frame_count {
        let mut buffer = vec![0; frame_bytes];
        let delay = decoder
            .read_frame(&mut buffer)
            .map_err(|_| ImageError::TransformFailed)?;
        delays_ms.push(delay);
        frames.push(Raster::new(width, height, layout, buffer)?);
    }

    let loop_count = match decoder.loop_count() {
        image_webp::LoopCount::Forever => 0,
        image_webp::LoopCount::Times(times) => times.get(),
    };
    Ok(Decoded {
        frames,
        delays_ms,
        loop_count,
    })
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
