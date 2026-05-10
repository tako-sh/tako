use super::*;
use image::{ImageBuffer, ImageFormat, Rgb, Rgba};
use std::io::Cursor;

const SECRET: &str = "test-image-secret";

fn private_options() -> ImageUrlOptions {
    ImageUrlOptions {
        source: "/assets/avatar.png".to_string(),
        width: 640,
        quality: 75,
        visibility: ImageVisibility::Private,
        expires_at_unix_secs: Some(1_900_000_000),
    }
}

#[test]
fn signs_path_based_image_urls() {
    let path = sign_image_path(SECRET, &private_options()).expect("sign path");

    assert!(path.starts_with("/_tako/image/v1/private/640/75/1900000000/"));
    assert!(!path.contains('?'));
}

#[test]
fn verifies_signed_private_path() {
    let path = sign_image_path(SECRET, &private_options()).expect("sign path");

    let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

    assert_eq!(verified.width, 640);
    assert_eq!(verified.quality, 75);
    assert_eq!(verified.visibility, ImageVisibility::Private);
    assert_eq!(
        verified.source,
        ImageSource::LocalPath("/assets/avatar.png".to_string())
    );
    assert_eq!(verified.expires_at_unix_secs, Some(1_900_000_000));
}

#[test]
fn public_paths_do_not_require_an_expiration() {
    let mut options = private_options();
    options.visibility = ImageVisibility::Public;
    options.expires_at_unix_secs = None;

    let path = sign_image_path(SECRET, &options).expect("sign path");
    let verified = verify_image_path(SECRET, &path, u64::MAX).expect("verify path");

    assert_eq!(verified.visibility, ImageVisibility::Public);
    assert_eq!(verified.expires_at_unix_secs, None);
}

#[test]
fn rejects_tampered_paths() {
    let path = sign_image_path(SECRET, &private_options()).expect("sign path");
    let tampered = path.replace("/640/", "/750/");

    let err = verify_image_path(SECRET, &tampered, 1_800_000_000).unwrap_err();

    assert_eq!(err, ImageError::InvalidSignature);
}

#[test]
fn rejects_expired_private_paths() {
    let path = sign_image_path(SECRET, &private_options()).expect("sign path");

    let err = verify_image_path(SECRET, &path, 1_900_000_001).unwrap_err();

    assert_eq!(err, ImageError::Expired);
}

#[test]
fn rejects_widths_outside_the_allowed_set() {
    let mut options = private_options();
    options.width = 641;

    let err = sign_image_path(SECRET, &options).unwrap_err();

    assert_eq!(err, ImageError::InvalidWidth);
}

#[test]
fn rejects_invalid_quality() {
    let mut options = private_options();
    options.quality = 0;

    let err = sign_image_path(SECRET, &options).unwrap_err();

    assert_eq!(err, ImageError::InvalidQuality);
}

#[test]
fn rejects_recursive_local_sources() {
    let mut options = private_options();
    options.source = "/_tako/image/v1/private/640/75/-/sig/source".to_string();

    let err = sign_image_path(SECRET, &options).unwrap_err();

    assert_eq!(err, ImageError::InvalidSource);
}

#[test]
fn rejects_remote_sources_with_userinfo_or_fragments() {
    for source in [
        "https://user@example.com/avatar.png",
        "https://example.com/avatar.png#fragment",
    ] {
        let mut options = private_options();
        options.source = source.to_string();

        let err = sign_image_path(SECRET, &options).unwrap_err();

        assert_eq!(err, ImageError::InvalidSource);
    }
}

#[test]
fn rejects_remote_sources_with_private_or_local_hosts() {
    for source in [
        "http://127.0.0.1/avatar.png",
        "http://[::1]/avatar.png",
        "http://localhost/avatar.png",
        "http://assets.localhost/avatar.png",
    ] {
        let mut options = private_options();
        options.source = source.to_string();

        let err = sign_image_path(SECRET, &options).unwrap_err();

        assert_eq!(err, ImageError::InvalidSource);
    }
}

#[test]
fn cache_control_is_private_by_default_and_public_by_opt_in() {
    assert_eq!(
        cache_control(ImageVisibility::Private),
        PRIVATE_CACHE_CONTROL
    );
    assert_eq!(cache_control(ImageVisibility::Public), PUBLIC_CACHE_CONTROL);
}

#[test]
fn resizes_png_without_upscaling() {
    let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([255_u8, 0, 0, 255]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Png)
        .expect("encode png");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/png"),
        16,
        80,
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 16);
    assert_eq!(transformed.height, 8);
    assert_eq!(transformed.content_type, "image/png");
    assert!(transformed.bytes.starts_with(&[0x89, b'P', b'N', b'G']));
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
        640,
        80,
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 32);
    assert_eq!(transformed.height, 16);
    assert_eq!(transformed.content_type, "image/png");
}

#[test]
fn jpeg_output_is_progressive_for_web_delivery() {
    let img = ImageBuffer::from_fn(80, 40, |_x, _y| Rgb([255_u8, 0, 0]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Jpeg)
        .expect("encode jpeg");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/jpeg"),
        16,
        80,
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.content_type, "image/jpeg");
    assert_eq!(jpeg_start_of_frame_marker(&transformed.bytes), Some(0xc2));
}

#[test]
fn resizes_webp_and_preserves_webp_output() {
    let img = ImageBuffer::from_fn(64, 32, |_x, _y| Rgba([32_u8, 96, 160, 255]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::WebP)
        .expect("encode webp");

    let transformed = transform_image(
        source.get_ref(),
        Some("image/webp"),
        16,
        80,
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
fn rejects_content_type_spoofed_unsupported_bytes() {
    let source = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="8"></svg>"#;

    let err = transform_image(
        source,
        Some("image/png"),
        16,
        80,
        &TransformLimits::default(),
    )
    .unwrap_err();

    assert_eq!(err, ImageError::UnsupportedFormat);
}

#[test]
fn applies_exif_orientation_when_no_resize_needed() {
    let img = ImageBuffer::from_fn(16, 8, |_x, _y| Rgb([255_u8, 0, 0]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Jpeg)
        .expect("encode jpeg");
    let source = jpeg_with_exif_orientation(source.get_ref(), 6);

    let transformed = transform_image(
        &source,
        Some("image/jpeg"),
        16,
        80,
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 8);
    assert_eq!(transformed.height, 16);
    assert_eq!(transformed.content_type, "image/jpeg");
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
        16,
        80,
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
        16,
        75,
        &TransformLimits {
            max_source_bytes: 8,
            ..Default::default()
        },
    )
    .unwrap_err();

    assert_eq!(err, ImageError::SourceTooLarge);
}

fn jpeg_start_of_frame_marker(bytes: &[u8]) -> Option<u8> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }

    let mut offset = 2;
    while offset + 3 < bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        if offset >= bytes.len() {
            return None;
        }

        let marker = bytes[offset];
        offset += 1;
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if offset + 2 > bytes.len() {
            return None;
        }

        let segment_len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        if segment_len < 2 || offset + segment_len > bytes.len() {
            return None;
        }
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            return Some(marker);
        }
        offset += segment_len;
    }
    None
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
