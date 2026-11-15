use crate::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use image::{ImageBuffer, ImageFormat, Rgb, Rgba};
use std::io::Cursor;

#[test]
fn resizes_png_to_avif() {
    let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([255_u8, 0, 0, 255]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/png"),
        transform_options(OutputFormat::Avif, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/avif");
    assert!(is_avif(&transformed.bytes));
}

#[test]
fn cover_crops_to_requested_box() {
    let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([255_u8, 0, 0, 255]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/png"),
        TransformOptions {
            height: Some(16),
            fit: Some(ImageFit::Cover),
            crop: Some(ImageCrop::Center),
            ..transform_options(OutputFormat::Avif, 16, 80)
        },
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 16);
    assert_eq!(transformed.content_type, "image/avif");
}

#[test]
fn contain_fits_inside_requested_box() {
    let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([255_u8, 0, 0, 255]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/png"),
        TransformOptions {
            height: Some(16),
            fit: Some(ImageFit::Contain),
            ..transform_options(OutputFormat::Avif, 16, 80)
        },
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/avif");
}

#[test]
fn smart_crop_uses_attention_strategy() {
    let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([255_u8, 0, 0, 255]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/png"),
        TransformOptions {
            height: Some(16),
            fit: Some(ImageFit::Cover),
            crop: Some(ImageCrop::Smart),
            ..transform_options(OutputFormat::Avif, 16, 80)
        },
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 16);
    assert_eq!(transformed.content_type, "image/avif");
}

#[test]
fn does_not_upscale_images_smaller_than_requested_width() {
    let img = ImageBuffer::from_fn(32, 16, |_x, _y| Rgba([0_u8, 128, 255, 255]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/png"),
        transform_options(OutputFormat::Avif, 640, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 32);
    assert_eq!(transformed.height, 16);
    assert_eq!(transformed.content_type, "image/avif");
}

#[test]
fn accepts_default_public_widths_at_transform_time() {
    let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([0_u8, 128, 255, 255]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    for width in [320, 960] {
        let transformed = transform_image(
            source.get_ref(),
            Some("image/png"),
            transform_options(OutputFormat::Webp, width, 80),
            &TransformLimits::default(),
        )
        .expect("transform image");

        assert_eq!(transformed.width, 64);
    }
}

#[test]
fn accepts_jpeg_sources_and_emits_avif() {
    let img = ImageBuffer::from_fn(80, 40, |_x, _y| Rgb([255_u8, 0, 0]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Jpeg)
        .expect("encode jpeg");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/jpeg"),
        transform_options(OutputFormat::Avif, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.content_type, "image/avif");
    assert!(is_avif(&transformed.bytes));
}

#[test]
fn resizes_large_jpeg_to_webp_without_dropping_lazy_pipeline_inputs() {
    let img = ImageBuffer::from_fn(1672, 941, |x, y| {
        Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
    });
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Jpeg)
        .expect("encode jpeg");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/jpeg"),
        transform_options(OutputFormat::Webp, 1200, 75),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 1200);
    assert_eq!(transformed.height, 675);
    assert_eq!(transformed.content_type, "image/webp");
    assert_eq!(&transformed.bytes[0..4], b"RIFF");
    assert_eq!(&transformed.bytes[8..12], b"WEBP");
}

#[test]
fn resizes_webp_sources_to_webp_when_requested() {
    let source = BASE64_STANDARD
        .decode(super::WEBP_64X32)
        .expect("decode webp");

    let transformed = transform_image(
        &source,
        Some("image/webp"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/webp");
    assert!(transformed.bytes.starts_with(b"RIFF"));
    assert_eq!(&transformed.bytes[8..12], b"WEBP");
}

#[test]
fn preserves_gif_animation_when_emitting_webp() {
    let source = BASE64_STANDARD
        .decode(super::GIF_32X16_TWO_FRAMES)
        .expect("decode gif");

    let transformed = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/webp");
    assert_animation_dimensions(&transformed.bytes, 16, 8, 2);
}

#[test]
fn preserves_webp_animation_when_emitting_webp() {
    let source = BASE64_STANDARD
        .decode(super::GIF_32X16_TWO_FRAMES)
        .expect("decode gif");
    let webp = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform gif to webp");

    let transformed = transform_image(
        &webp.bytes,
        Some("image/webp"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/webp");
    assert_animation_dimensions(&transformed.bytes, 16, 8, 2);
}

#[test]
fn falls_back_to_webp_for_gif_animation_when_avif_requested() {
    let source = BASE64_STANDARD
        .decode(super::GIF_32X16_TWO_FRAMES)
        .expect("decode gif");

    let transformed = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Avif, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/webp");
    assert_eq!(transformed.format, OutputFormat::Webp);
    assert!(transformed.bytes.starts_with(b"RIFF"));
    assert_eq!(&transformed.bytes[8..12], b"WEBP");
    assert_animation_dimensions(&transformed.bytes, 16, 8, 2);
}

#[test]
fn falls_back_to_webp_for_webp_animation_when_avif_requested() {
    let source = BASE64_STANDARD
        .decode(super::GIF_32X16_TWO_FRAMES)
        .expect("decode gif");
    let webp = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform gif to webp");

    let transformed = transform_image(
        &webp.bytes,
        Some("image/webp"),
        transform_options(OutputFormat::Avif, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/webp");
    assert_eq!(transformed.format, OutputFormat::Webp);
    assert!(transformed.bytes.starts_with(b"RIFF"));
    assert_eq!(&transformed.bytes[8..12], b"WEBP");
    assert_animation_dimensions(&transformed.bytes, 16, 8, 2);
}

#[test]
fn center_cover_crops_gif_animation_when_emitting_webp() {
    let source = BASE64_STANDARD
        .decode(super::GIF_32X16_TWO_FRAMES)
        .expect("decode gif");

    let transformed = transform_image(
        &source,
        Some("image/gif"),
        TransformOptions {
            height: Some(16),
            fit: Some(ImageFit::Cover),
            crop: Some(ImageCrop::Center),
            ..transform_options(OutputFormat::Webp, 16, 80)
        },
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 16);
    assert_eq!(transformed.content_type, "image/webp");
    assert_animation_dimensions(&transformed.bytes, 16, 16, 2);
}

#[test]
fn falls_back_to_webp_for_cropped_gif_animation_when_avif_requested() {
    let source = BASE64_STANDARD
        .decode(super::GIF_32X16_TWO_FRAMES)
        .expect("decode gif");

    let transformed = transform_image(
        &source,
        Some("image/gif"),
        TransformOptions {
            height: Some(16),
            fit: Some(ImageFit::Cover),
            crop: Some(ImageCrop::Smart),
            ..transform_options(OutputFormat::Avif, 16, 80)
        },
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 16);
    assert_eq!(transformed.content_type, "image/webp");
    assert_eq!(transformed.format, OutputFormat::Webp);
    assert!(transformed.bytes.starts_with(b"RIFF"));
    assert_eq!(&transformed.bytes[8..12], b"WEBP");
    assert_animation_dimensions(&transformed.bytes, 16, 16, 2);
}

#[test]
fn accepts_avif_sources_and_emits_webp_when_requested() {
    let source = BASE64_STANDARD
        .decode(super::AVIF_16X8)
        .expect("decode avif");

    let transformed = transform_image(
        &source,
        Some("image/avif"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/webp");
}

#[test]
fn rejects_content_type_spoofed_unsupported_bytes() {
    let source = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="8"></svg>"#;

    let err = transform_image(
        source,
        Some("image/png"),
        transform_options(OutputFormat::Avif, 16, 80),
        &TransformLimits::default(),
    )
    .unwrap_err();

    assert_eq!(err, ImageError::UnsupportedFormat);
}

#[test]
fn applies_exif_orientation_when_no_resize_needed() {
    let img = ImageBuffer::from_fn(32, 16, |_x, _y| Rgb([255_u8, 0, 0]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Jpeg)
        .expect("encode jpeg");
    let source = jpeg_with_exif_orientation(source.get_ref(), 6);

    let transformed = transform_image(
        &source,
        Some("image/jpeg"),
        transform_options(OutputFormat::Avif, 32, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 32);
    assert_eq!(transformed.content_type, "image/avif");
}

#[test]
fn strips_source_metadata_after_applying_orientation() {
    let img = ImageBuffer::from_fn(32, 16, |_x, _y| Rgb([255_u8, 0, 0]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Jpeg)
        .expect("encode jpeg");
    let source = jpeg_with_private_metadata(source.get_ref(), 6);
    assert_contains_bytes(&source, PRIVATE_XMP_MARKER);
    assert_contains_bytes(&source, b"Exif\0\0");

    for format in [OutputFormat::Avif, OutputFormat::Webp] {
        let transformed = transform_image(
            &source,
            Some("image/jpeg"),
            transform_options(format, 32, 80),
            &TransformLimits::default(),
        )
        .expect("transform image");

        assert_eq!(transformed.width, 16);
        assert_eq!(transformed.height, 32);
        assert_no_private_metadata(&transformed.bytes);
    }
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
        transform_options(OutputFormat::Avif, 16, 80),
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
        transform_options(OutputFormat::Avif, 16, 75),
        &TransformLimits {
            max_source_bytes: 8,
            ..Default::default()
        },
    )
    .unwrap_err();

    assert_eq!(err, ImageError::SourceTooLarge);
}

#[test]
fn converts_gif_repeat_count_to_webp_play_count() {
    let source = gif_animation(Some(gif::Repeat::Finite(3)), &[4, 7, 11]);

    let transformed = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 32, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    let animation = decode_webp_animation(&transformed.bytes);
    // GIF repeats three extra times; WebP counts all four plays.
    assert_eq!(
        animation.loop_count,
        image_webp::LoopCount::Times(4.try_into().expect("non-zero"))
    );
    assert_eq!(animation.delays_ms, [40, 70, 110]);
}

#[test]
fn plays_gif_without_loop_extension_once() {
    let source = gif_animation(None, &[5, 5]);

    let transformed = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 32, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(
        decode_webp_animation(&transformed.bytes).loop_count,
        image_webp::LoopCount::Times(1.try_into().expect("non-zero"))
    );
}

#[test]
fn preserves_infinite_gif_loop_and_webp_loop_count() {
    let source = BASE64_STANDARD
        .decode(super::GIF_32X16_TWO_FRAMES)
        .expect("decode gif");
    let looping = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");
    assert_eq!(
        decode_webp_animation(&looping.bytes).loop_count,
        image_webp::LoopCount::Forever
    );

    let finite = transform_image(
        &gif_animation(Some(gif::Repeat::Finite(1)), &[5, 5]),
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 32, 80),
        &TransformLimits::default(),
    )
    .expect("transform gif");
    let rewritten = transform_image(
        &finite.bytes,
        Some("image/webp"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform webp");
    assert_eq!(
        decode_webp_animation(&rewritten.bytes).loop_count,
        image_webp::LoopCount::Times(2.try_into().expect("non-zero"))
    );
}

#[test]
fn keeps_transparent_canvas_in_animated_webp() {
    // Regression: libwebp's animation encoder trimmed the transparent left
    // half out of every sub-frame and dropped the VP8X alpha flag, so
    // decoders painted it opaque.
    let source = gif_animation(Some(gif::Repeat::Infinite), &[4, 7, 11, 13]);

    let transformed = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 32, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    let animation = decode_webp_animation(&transformed.bytes);
    assert!(animation.has_alpha, "animated WebP lost its alpha flag");
    assert_eq!(animation.frames.len(), 4);
    for frame in &animation.frames {
        for y in 0..16usize {
            for x in 0..32usize {
                let alpha = frame[(y * 32 + x) * 4 + 3];
                if x < 12 {
                    assert_eq!(alpha, 0, "pixel ({x}, {y}) should stay transparent");
                } else if x >= 20 {
                    assert_eq!(alpha, 255, "pixel ({x}, {y}) should stay opaque");
                }
            }
        }
    }
}

#[test]
fn accepts_zero_delay_gif_frames() {
    let source = gif_animation(Some(gif::Repeat::Infinite), &[0, 0, 3]);

    let transformed = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_animation_dimensions(&transformed.bytes, 16, 8, 3);
}

#[test]
fn rejects_animations_above_cumulative_frame_budget() {
    let source = gif_animation(Some(gif::Repeat::Infinite), &[5, 5, 5]);
    // Each 32x16 frame is 512 pixels; two fit, three do not.
    let limits = TransformLimits {
        max_decoded_pixels: 1_100,
        ..Default::default()
    };

    let err = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 16, 80),
        &limits,
    )
    .unwrap_err();
    assert_eq!(err, ImageError::ImageTooLarge);

    let webp = transform_image(
        &source,
        Some("image/gif"),
        transform_options(OutputFormat::Webp, 32, 80),
        &TransformLimits::default(),
    )
    .expect("transform gif");
    let err = transform_image(
        &webp.bytes,
        Some("image/webp"),
        transform_options(OutputFormat::Webp, 16, 80),
        &limits,
    )
    .unwrap_err();
    assert_eq!(err, ImageError::ImageTooLarge);
}

#[test]
fn round_trips_transparent_images_through_avif() {
    let img = ImageBuffer::from_fn(64, 32, |x, _y| {
        Rgba([0_u8, 96, 255, if x < 32 { 0 } else { 255 }])
    });
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    let avif = transform_image(
        source.get_ref(),
        Some("image/png"),
        transform_options(OutputFormat::Avif, 64, 80),
        &TransformLimits::default(),
    )
    .expect("encode avif");
    assert!(is_avif(&avif.bytes));

    let webp = transform_image(
        &avif.bytes,
        Some("image/avif"),
        transform_options(OutputFormat::Webp, 32, 90),
        &TransformLimits::default(),
    )
    .expect("decode avif");
    assert_eq!((webp.width, webp.height), (32, 16));

    let decoded = image::load_from_memory_with_format(&webp.bytes, ImageFormat::WebP)
        .expect("decode webp")
        .into_rgba8();
    assert!(
        decoded.get_pixel(2, 8)[3] < 16,
        "left half should stay transparent"
    );
    assert!(
        decoded.get_pixel(29, 8)[3] > 240,
        "right half should stay opaque"
    );
}

#[test]
fn rejects_avif_above_dimension_limits_before_decoding() {
    let source = BASE64_STANDARD
        .decode(super::AVIF_16X8)
        .expect("decode avif");

    let err = transform_image(
        &source,
        Some("image/avif"),
        transform_options(OutputFormat::Webp, 16, 80),
        &TransformLimits {
            max_image_width: 8,
            ..Default::default()
        },
    )
    .unwrap_err();

    assert_eq!(err, ImageError::ImageTooLarge);
}

#[test]
fn smart_crop_keeps_the_detailed_region() {
    // Flat gray with a saturated checkerboard on the right edge.
    let img = ImageBuffer::from_fn(256, 64, |x, y| {
        if x >= 192 {
            if (x / 8 + y / 8) % 2 == 0 {
                Rgb([255_u8, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        } else {
            Rgb([128, 128, 128])
        }
    });
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    let crop = |crop| {
        let transformed = transform_image(
            source.get_ref(),
            Some("image/png"),
            TransformOptions {
                height: Some(64),
                fit: Some(ImageFit::Cover),
                crop: Some(crop),
                ..transform_options(OutputFormat::Webp, 64, 90)
            },
            &TransformLimits::default(),
        )
        .expect("transform image");
        assert_eq!((transformed.width, transformed.height), (64, 64));
        image::load_from_memory_with_format(&transformed.bytes, ImageFormat::WebP)
            .expect("decode webp")
            .into_rgb8()
    };

    let saturated = |image: &image::RgbImage| {
        image
            .pixels()
            .filter(|pixel| pixel[0].abs_diff(pixel[2]) > 128)
            .count()
    };
    let center = crop(ImageCrop::Center);
    let smart = crop(ImageCrop::Smart);
    assert_eq!(saturated(&center), 0);
    let smart_saturated = saturated(&smart);
    assert!(
        smart_saturated > 3_000,
        "smart crop missed the detail: {smart_saturated} saturated pixels"
    );
}

/// A 32x16 animation: the left 12 columns are transparent, the right 12 are
/// opaque and change color per frame, and the middle uses partial frames.
fn gif_animation(repeat: Option<gif::Repeat>, delays_cs: &[u16]) -> Vec<u8> {
    let palette = [0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
    let mut bytes = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut bytes, 32, 16, &palette).expect("gif encoder");
        if let Some(repeat) = repeat {
            encoder.set_repeat(repeat).expect("set repeat");
        }
        for (index, delay) in delays_cs.iter().enumerate() {
            let color = 1 + (index % 4) as u8;
            let pixels = (0..16 * 32)
                .map(|i| if i % 32 < 12 { 0 } else { color })
                .collect::<Vec<u8>>();
            let mut frame = gif::Frame::from_indexed_pixels(32, 16, pixels, Some(0));
            frame.delay = *delay;
            frame.dispose = gif::DisposalMethod::Background;
            encoder.write_frame(&frame).expect("write frame");
        }
    }
    bytes
}

fn transform_options(format: OutputFormat, width: u32, quality: u8) -> TransformOptions {
    TransformOptions {
        format,
        width,
        height: None,
        fit: None,
        crop: None,
        quality,
    }
}

fn is_avif(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && bytes[8..]
            .windows(4)
            .any(|brand| brand == b"avif" || brand == b"avis")
}

fn assert_animation_dimensions(bytes: &[u8], width: u32, height: u32, frames: u32) {
    let animation = decode_webp_animation(bytes);
    assert_eq!((animation.width, animation.height), (width, height));
    assert_eq!(animation.frames.len(), frames as usize);
}

struct WebpAnimation {
    width: u32,
    height: u32,
    has_alpha: bool,
    loop_count: image_webp::LoopCount,
    delays_ms: Vec<u32>,
    /// RGBA canvas after each frame.
    frames: Vec<Vec<u8>>,
}

fn decode_webp_animation(bytes: &[u8]) -> WebpAnimation {
    let mut decoder = image_webp::WebPDecoder::new(Cursor::new(bytes)).expect("parse webp");
    assert!(decoder.is_animated(), "expected an animated WebP");
    let (width, height) = decoder.dimensions();
    let has_alpha = decoder.has_alpha();
    let loop_count = decoder.loop_count();
    let mut delays_ms = Vec::new();
    let mut frames = Vec::new();
    for _ in 0..decoder.num_frames() {
        let mut buffer = vec![0; decoder.output_buffer_size().expect("buffer size")];
        delays_ms.push(decoder.read_frame(&mut buffer).expect("decode frame"));
        if !has_alpha {
            buffer = buffer
                .chunks_exact(3)
                .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
                .collect();
        }
        frames.push(buffer);
    }
    WebpAnimation {
        width,
        height,
        has_alpha,
        loop_count,
        delays_ms,
        frames,
    }
}

const PRIVATE_XMP_MARKER: &[u8] = b"tako-private-xmp-marker";

fn assert_no_private_metadata(bytes: &[u8]) {
    for marker in [
        b"Exif\0\0".as_slice(),
        b"Exif".as_slice(),
        b"EXIF".as_slice(),
        b"XMP ".as_slice(),
        b"ICCP".as_slice(),
        b"http://ns.adobe.com/xap/1.0/".as_slice(),
        PRIVATE_XMP_MARKER,
    ] {
        assert!(
            !contains_bytes(bytes, marker),
            "transformed image retained metadata marker {marker:?}"
        );
    }
}

fn assert_contains_bytes(bytes: &[u8], marker: &[u8]) {
    assert!(
        contains_bytes(bytes, marker),
        "test fixture is missing metadata marker {marker:?}"
    );
}

fn contains_bytes(bytes: &[u8], marker: &[u8]) -> bool {
    bytes.windows(marker.len()).any(|window| window == marker)
}

fn jpeg_with_private_metadata(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let jpeg = jpeg_with_exif_orientation(jpeg, orientation);
    jpeg_with_xmp_packet(&jpeg)
}

fn jpeg_with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    assert!(jpeg.starts_with(&[0xff, 0xd8]));

    let mut exif = Vec::new();
    exif.extend_from_slice(b"Exif\0\0");
    exif.extend_from_slice(b"MM");
    exif.extend_from_slice(&42_u16.to_be_bytes());
    exif.extend_from_slice(&8_u32.to_be_bytes());
    exif.extend_from_slice(&1_u16.to_be_bytes());
    exif.extend_from_slice(&0x0112_u16.to_be_bytes());
    exif.extend_from_slice(&3_u16.to_be_bytes());
    exif.extend_from_slice(&1_u32.to_be_bytes());
    exif.extend_from_slice(&orientation.to_be_bytes());
    exif.extend_from_slice(&0_u16.to_be_bytes());
    exif.extend_from_slice(&0_u32.to_be_bytes());

    let segment_len = u16::try_from(exif.len() + 2).expect("exif segment fits");
    let mut output = Vec::with_capacity(jpeg.len() + exif.len() + 4);
    output.extend_from_slice(&jpeg[..2]);
    output.extend_from_slice(&[0xff, 0xe1]);
    output.extend_from_slice(&segment_len.to_be_bytes());
    output.extend_from_slice(&exif);
    output.extend_from_slice(&jpeg[2..]);
    output
}

fn jpeg_with_xmp_packet(jpeg: &[u8]) -> Vec<u8> {
    assert!(jpeg.starts_with(&[0xff, 0xd8]));

    let mut xmp = Vec::new();
    xmp.extend_from_slice(b"http://ns.adobe.com/xap/1.0/\0");
    xmp.extend_from_slice(br#"<?xpacket begin=""?><x:xmpmeta xmlns:x="adobe:ns:meta/">"#);
    xmp.extend_from_slice(PRIVATE_XMP_MARKER);
    xmp.extend_from_slice(br#"</x:xmpmeta><?xpacket end="w"?>"#);

    let segment_len = u16::try_from(xmp.len() + 2).expect("xmp segment fits");
    let mut output = Vec::with_capacity(jpeg.len() + xmp.len() + 4);
    output.extend_from_slice(&jpeg[..2]);
    output.extend_from_slice(&[0xff, 0xe1]);
    output.extend_from_slice(&segment_len.to_be_bytes());
    output.extend_from_slice(&xmp);
    output.extend_from_slice(&jpeg[2..]);
    output
}
