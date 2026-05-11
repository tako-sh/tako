use super::*;
use base64::Engine;
use base64::engine::general_purpose::{
    STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD,
};
use image::{ImageBuffer, ImageFormat, Rgb, Rgba};
use serde_json::{Value, json};
use std::io::Cursor;
use std::time::Duration;

const SECRET: &str = "test-image-secret";
const WEBP_64X32: &str =
    "UklGRjYAAABXRUJQVlA4ICoAAAAQAwCdASpAACAAPpFIn0ulpCKhpAgAsBIJaQAAH2A8tGAA/vjO9XgAAAA=";
const AVIF_16X8: &str = "AAAAHGZ0eXBhdmlmAAAAAG1pZjFhdmlmbWlhZgAAARdtZXRhAAAAAAAAACFoZGxyAAAAAAAAAABwaWN0AAAAAAAAAAAAAAAAAAAAAA5waXRtAAAAAAABAAAANGlsb2MAAAAAREAAAgABAAAAAAE7AAEAAAAAAAAAGAACAAAAAAFTAAEAAAAAAAAAvgAAADhpaW5mAAAAAAACAAAAFWluZmUCAAAAAAEAAGF2MDEAAAAAFWluZmUCAAABAAIAAEV4aWYAAAAAVmlwcnAAAAA4aXBjbwAAAAxhdjFDgQAMAAAAABRpc3BlAAAAAAAAABAAAAAIAAAAEHBpeGkAAAAAAwgICAAAABZpcG1hAAAAAAAAAAEAAQOBAgMAAAAaaXJlZgAAAAAAAAAOY2RzYwACAAEAAQAAAN5tZGF0EgAKCBgMv7BAQ0GhMgoYAAAAQADDXsl8AAAABkV4aWYAAElJKgAIAAAABgASAQMAAQAAAAEAAAAaAQUAAQAAAFYAAAAbAQUAAQAAAF4AAAAoAQMAAQAAAAIAAAATAgMAAQAAAAEAAABphwQAAQAAAGYAAAAAAAAAOGMAAOgDAAA4YwAA6AMAAAYAAJAHAAQAAAAwMjEwAZEHAAQAAAABAgMAAKAHAAQAAAAwMTAwAaADAAEAAAD//wAAAqAEAAEAAAAQAAAAA6AEAAEAAAAIAAAAAAAAAA==";

fn private_options() -> ImageUrlOptions {
    ImageUrlOptions {
        source: "/assets/avatar.png".to_string(),
        format: OutputFormat::Avif,
        width: Some(640),
        height: None,
        fit: None,
        crop: None,
        quality: 75,
        visibility: ImageVisibility::Private,
        expires_at_unix_secs: Some(1_900_000_000),
        private_browser_cache_max_age: None,
    }
}

#[test]
fn signs_path_based_image_urls() {
    let path = sign_image_path(SECRET, &private_options()).expect("sign path");

    assert!(path.starts_with("/_tako/image/v1/"));
    assert!(!path.contains('?'));
    assert_eq!(
        payload_value(&path),
        json!({
            "w": 640,
            "e": 1_900_000_000_u64,
            "s": "/assets/avatar.png",
        })
    );
}

#[test]
fn signs_default_width_paths_without_width_payload() {
    let mut options = private_options();
    options.width = None;

    let path = sign_image_path(SECRET, &options).expect("sign path");
    let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

    assert_eq!(verified.width, 1200);
    assert_eq!(
        payload_value(&path),
        json!({
            "e": 1_900_000_000_u64,
            "s": "/assets/avatar.png",
        })
    );
}

#[test]
fn verifies_signed_private_path() {
    let path = sign_image_path(SECRET, &private_options()).expect("sign path");

    let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

    assert_eq!(verified.width, 640);
    assert_eq!(verified.height, None);
    assert_eq!(verified.fit, None);
    assert_eq!(verified.crop, None);
    assert_eq!(verified.quality, 75);
    assert_eq!(verified.format, OutputFormat::Avif);
    assert_eq!(verified.visibility, ImageVisibility::Private);
    assert_eq!(
        verified.private_browser_cache_max_age,
        Some(DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE)
    );
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
    assert_eq!(verified.private_browser_cache_max_age, None);
    assert_eq!(
        payload_value(&path),
        json!({
            "pub": true,
            "w": 640,
            "s": "/assets/avatar.png",
        })
    );
}

#[test]
fn signs_cover_crop_paths() {
    let mut options = private_options();
    options.height = Some(640);
    options.crop = Some(ImageCrop::Smart);

    let path = sign_image_path(SECRET, &options).expect("sign path");
    let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

    assert_eq!(verified.height, Some(640));
    assert_eq!(verified.fit, Some(ImageFit::Cover));
    assert_eq!(verified.crop, Some(ImageCrop::Smart));
    assert_eq!(
        payload_value(&path),
        json!({
            "w": 640,
            "h": 640,
            "crop": "smart",
            "e": 1_900_000_000_u64,
            "s": "/assets/avatar.png",
        })
    );
}

#[test]
fn signs_contain_paths_without_crop() {
    let mut options = private_options();
    options.width = Some(384);
    options.height = Some(256);
    options.fit = Some(ImageFit::Contain);

    let path = sign_image_path(SECRET, &options).expect("sign path");
    let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

    assert_eq!(verified.height, Some(256));
    assert_eq!(verified.fit, Some(ImageFit::Contain));
    assert_eq!(verified.crop, None);
    assert_eq!(
        payload_value(&path),
        json!({
            "w": 384,
            "h": 256,
            "fit": "contain",
            "e": 1_900_000_000_u64,
            "s": "/assets/avatar.png",
        })
    );
}

#[test]
fn rejects_incompatible_resize_options() {
    let mut no_height = private_options();
    no_height.crop = Some(ImageCrop::Smart);
    assert_eq!(
        sign_image_path(SECRET, &no_height).unwrap_err(),
        ImageError::InvalidResize
    );

    let mut contain_with_crop = private_options();
    contain_with_crop.height = Some(640);
    contain_with_crop.fit = Some(ImageFit::Contain);
    contain_with_crop.crop = Some(ImageCrop::Smart);
    assert_eq!(
        sign_image_path(SECRET, &contain_with_crop).unwrap_err(),
        ImageError::InvalidResize
    );

    let mut invalid_height = private_options();
    invalid_height.height = Some(641);
    assert_eq!(
        sign_image_path(SECRET, &invalid_height).unwrap_err(),
        ImageError::InvalidHeight
    );

    let mut height_without_width = private_options();
    height_without_width.width = None;
    height_without_width.height = Some(640);
    assert_eq!(
        sign_image_path(SECRET, &height_without_width).unwrap_err(),
        ImageError::InvalidResize
    );
}

#[test]
fn private_paths_can_override_browser_cache_max_age() {
    let mut options = private_options();
    options.private_browser_cache_max_age = Some(Duration::from_secs(3_600));

    let path = sign_image_path(SECRET, &options).expect("sign path");
    let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

    assert_eq!(
        verified.private_browser_cache_max_age,
        Some(Duration::from_secs(3_600))
    );
    assert_eq!(
        payload_value(&path),
        json!({
            "w": 640,
            "c": 3_600_u64,
            "e": 1_900_000_000_u64,
            "s": "/assets/avatar.png",
        })
    );
}

#[test]
fn public_paths_reject_browser_cache_overrides() {
    let mut options = private_options();
    options.visibility = ImageVisibility::Public;
    options.expires_at_unix_secs = None;
    options.private_browser_cache_max_age = Some(Duration::from_secs(3_600));

    let err = sign_image_path(SECRET, &options).unwrap_err();

    assert_eq!(err, ImageError::InvalidBrowserCacheMaxAge);
}

#[test]
fn signs_webp_fallback_urls() {
    let mut options = private_options();
    options.format = OutputFormat::Webp;

    let path = sign_image_path(SECRET, &options).expect("sign path");
    let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

    assert_eq!(verified.format, OutputFormat::Webp);
    assert_eq!(
        payload_value(&path),
        json!({
            "f": "webp",
            "w": 640,
            "e": 1_900_000_000_u64,
            "s": "/assets/avatar.png",
        })
    );
}

#[test]
fn rejects_tampered_paths() {
    let path = sign_image_path(SECRET, &private_options()).expect("sign path");
    let tampered = tamper_payload(&path, |payload| payload["w"] = json!(750));

    let err = verify_image_path(SECRET, &tampered, 1_800_000_000).unwrap_err();

    assert_eq!(err, ImageError::InvalidSignature);
}

#[test]
fn rejects_tampered_output_formats() {
    let path = sign_image_path(SECRET, &private_options()).expect("sign path");
    let tampered = tamper_payload(&path, |payload| payload["f"] = json!("webp"));

    let err = verify_image_path(SECRET, &tampered, 1_800_000_000).unwrap_err();

    assert_eq!(err, ImageError::InvalidSignature);
}

#[test]
fn rejects_explicit_avif_payload_formats() {
    let path = signed_payload(json!({
        "f": "avif",
        "w": 640,
        "e": 1_900_000_000_u64,
        "s": "/assets/avatar.png",
    }));

    let err = verify_image_path(SECRET, &path, 1_800_000_000).unwrap_err();

    assert_eq!(err, ImageError::UnsupportedFormat);
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
    options.width = Some(641);

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
fn rejects_invalid_private_browser_cache_max_age() {
    for max_age in [
        Duration::ZERO,
        MAX_PRIVATE_BROWSER_CACHE_MAX_AGE + Duration::from_secs(1),
    ] {
        let mut options = private_options();
        options.private_browser_cache_max_age = Some(max_age);

        let err = sign_image_path(SECRET, &options).unwrap_err();

        assert_eq!(err, ImageError::InvalidBrowserCacheMaxAge);
    }
}

#[test]
fn rejects_recursive_local_sources() {
    let mut options = private_options();
    options.source = "/_tako/image/v1/payload.sig".to_string();

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
        cache_control(ImageVisibility::Private, None),
        PRIVATE_CACHE_CONTROL
    );
    assert_eq!(
        cache_control(ImageVisibility::Private, Some(Duration::from_secs(3_600))),
        "private, max-age=3600"
    );
    assert_eq!(
        cache_control(ImageVisibility::Public, Some(Duration::from_secs(3_600))),
        PUBLIC_CACHE_CONTROL
    );
}

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
    let source = BASE64_STANDARD.decode(WEBP_64X32).expect("decode webp");

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
    let source = BASE64_STANDARD.decode(AVIF_16X8).expect("decode avif");

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
    let img = ImageBuffer::from_fn(16, 8, |_x, _y| Rgb([255_u8, 0, 0]));
    let mut source = Cursor::new(Vec::new());
    img.write_to(&mut source, ImageFormat::Jpeg)
        .expect("encode jpeg");
    let source = jpeg_with_exif_orientation(source.get_ref(), 6);

    let transformed = transform_image(
        &source,
        Some("image/jpeg"),
        transform_options(OutputFormat::Avif, 16, 80),
        &TransformLimits::default(),
    )
    .expect("transform image");

    assert_eq!(transformed.width, 8);
    assert_eq!(transformed.height, 16);
    assert_eq!(transformed.content_type, "image/avif");
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

fn payload_value(path: &str) -> Value {
    let token = path
        .strip_prefix("/_tako/image/v1/")
        .expect("image URL prefix");
    assert!(!token.contains('/'));
    let (payload, signature) = token.split_once('.').expect("payload and signature");
    assert!(!payload.is_empty());
    assert!(!signature.is_empty());
    let bytes = BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .expect("base64url payload");
    serde_json::from_slice(&bytes).expect("json payload")
}

fn tamper_payload(path: &str, mutate: impl FnOnce(&mut Value)) -> String {
    let token = path
        .strip_prefix("/_tako/image/v1/")
        .expect("image URL prefix");
    let (_, signature) = token.split_once('.').expect("payload and signature");
    let mut decoded = payload_value(path);
    mutate(&mut decoded);
    let encoded = BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&decoded).expect("json"));
    format!("/_tako/image/v1/{encoded}.{signature}")
}

fn signed_payload(payload: Value) -> String {
    let encoded = BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("json"));
    let signature = signature(SECRET, &encoded).expect("sign payload");
    format!("/_tako/image/v1/{encoded}.{signature}")
}

fn is_avif(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && bytes[8..].windows(4).any(|brand| brand == b"avif")
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
