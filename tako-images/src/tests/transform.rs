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
        && bytes[8..].windows(4).any(|brand| brand == b"avif")
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
