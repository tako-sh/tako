use super::*;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use std::time::Duration;

mod transform;

const SECRET: &str = "test-image-secret";
const WEBP_64X32: &str =
    "UklGRjYAAABXRUJQVlA4ICoAAAAQAwCdASpAACAAPpFIn0ulpCKhpAgAsBIJaQAAH2A8tGAA/vjO9XgAAAA=";
const AVIF_16X8: &str = "AAAAHGZ0eXBhdmlmAAAAAG1pZjFhdmlmbWlhZgAAARdtZXRhAAAAAAAAACFoZGxyAAAAAAAAAABwaWN0AAAAAAAAAAAAAAAAAAAAAA5waXRtAAAAAAABAAAANGlsb2MAAAAAREAAAgABAAAAAAE7AAEAAAAAAAAAGAACAAAAAAFTAAEAAAAAAAAAvgAAADhpaW5mAAAAAAACAAAAFWluZmUCAAAAAAEAAGF2MDEAAAAAFWluZmUCAAABAAIAAEV4aWYAAAAAVmlwcnAAAAA4aXBjbwAAAAxhdjFDgQAMAAAAABRpc3BlAAAAAAAAABAAAAAIAAAAEHBpeGkAAAAAAwgICAAAABZpcG1hAAAAAAAAAAEAAQOBAgMAAAAaaXJlZgAAAAAAAAAOY2RzYwACAAEAAQAAAN5tZGF0EgAKCBgMv7BAQ0GhMgoYAAAAQADDXsl8AAAABkV4aWYAAElJKgAIAAAABgASAQMAAQAAAAEAAAAaAQUAAQAAAFYAAAAbAQUAAQAAAF4AAAAoAQMAAQAAAAIAAAATAgMAAQAAAAEAAABphwQAAQAAAGYAAAAAAAAAOGMAAOgDAAA4YwAA6AMAAAYAAJAHAAQAAAAwMjEwAZEHAAQAAAABAgMAAKAHAAQAAAAwMTAwAaADAAEAAAD//wAAAqAEAAEAAAAQAAAAA6AEAAEAAAAIAAAAAAAAAA==";
const GIF_32X16_TWO_FRAMES: &str = "R0lGODlhIAAQAPAAAP8AAAAAACH/C05FVFNDQVBFMi4wAwEAAAAh+QQAAAAAACwAAAAAIAAQAAACFISPqcvtD6OctNqLs968+w+G4kgVACH5BAAAAAAALAAAAAAgABAAgAAA/wAAAAIUhI+py+0Po5y02ouz3rz7D4biSBUAOw==";

fn private_options() -> ImageUrlOptions {
    ImageUrlOptions {
        source: "/assets/avatar.png".to_string(),
        format: OutputFormat::Webp,
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
    assert_eq!(verified.format, OutputFormat::Webp);
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
fn signs_avif_urls() {
    let mut options = private_options();
    options.format = OutputFormat::Avif;

    let path = sign_image_path(SECRET, &options).expect("sign path");
    let verified = verify_image_path(SECRET, &path, 1_800_000_000).expect("verify path");

    assert_eq!(verified.format, OutputFormat::Avif);
    assert_eq!(
        payload_value(&path),
        json!({
            "f": "avif",
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
    let tampered = tamper_payload(&path, |payload| payload["f"] = json!("avif"));

    let err = verify_image_path(SECRET, &tampered, 1_800_000_000).unwrap_err();

    assert_eq!(err, ImageError::InvalidSignature);
}

#[test]
fn rejects_explicit_webp_payload_formats() {
    let path = signed_payload(json!({
        "f": "webp",
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
fn verifies_public_local_image_requests_with_default_config() {
    let verified = verify_public_image_request(
        "/_tako/image",
        Some("src=%2Fassets%2Favatar.png&w=640"),
        Some("image/avif,image/webp"),
        &ImagesConfig::default(),
    )
    .expect("verify public image");

    assert_eq!(
        verified.source,
        ImageSource::LocalPath("/assets/avatar.png".to_string())
    );
    assert_eq!(verified.width, 640);
    assert_eq!(verified.quality, 75);
    assert_eq!(verified.format, OutputFormat::Webp);
    assert_eq!(verified.visibility, ImageVisibility::Public);
    assert!(verified.vary_accept);
}

#[test]
fn public_format_negotiation_respects_configured_order() {
    let avif_first = ImagesConfig {
        formats: vec![OutputFormat::Avif, OutputFormat::Webp],
        ..Default::default()
    };
    let webp_first = ImagesConfig {
        formats: vec![OutputFormat::Webp, OutputFormat::Avif],
        ..Default::default()
    };

    let avif = verify_public_image_request(
        "/_tako/image",
        Some("src=%2Fassets%2Favatar.png&w=640"),
        Some("image/avif,image/webp"),
        &avif_first,
    )
    .expect("verify public avif image");
    let webp = verify_public_image_request(
        "/_tako/image",
        Some("src=%2Fassets%2Favatar.png&w=640"),
        Some("image/avif,image/webp"),
        &webp_first,
    )
    .expect("verify public webp image");

    assert_eq!(avif.format, OutputFormat::Avif);
    assert_eq!(webp.format, OutputFormat::Webp);
}

#[test]
fn public_local_patterns_override_default_local_access() {
    let config = ImagesConfig {
        local_patterns: Some(vec!["/images/**".to_string()]),
        ..Default::default()
    };

    verify_public_image_request(
        "/_tako/image",
        Some("src=%2Fimages%2Favatar.png&w=640"),
        None,
        &config,
    )
    .expect("matching local pattern");

    let err = verify_public_image_request(
        "/_tako/image",
        Some("src=%2Fassets%2Favatar.png&w=640"),
        None,
        &config,
    )
    .unwrap_err();

    assert_eq!(err, ImageError::InvalidSignature);
}

#[test]
fn verifies_public_remote_image_requests_against_remote_patterns() {
    let config = ImagesConfig {
        remote_patterns: vec![
            "https://cdn.example.com/uploads/**".to_string(),
            "*.assets.example.com/images/*".to_string(),
        ],
        ..Default::default()
    };

    let verified = verify_public_image_request(
        "/_tako/image",
        Some("src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatar.jpg%3Fv%3D1&w=960&f=webp"),
        Some("image/avif"),
        &config,
    )
    .expect("matching remote pattern");

    assert_eq!(
        verified.source,
        ImageSource::RemoteUrl("https://cdn.example.com/uploads/avatar.jpg?v=1".to_string())
    );
    assert_eq!(verified.format, OutputFormat::Webp);
    assert!(!verified.vary_accept);

    let err = verify_public_image_request(
        "/_tako/image",
        Some("src=https%3A%2F%2Fevil.example.com%2Fuploads%2Favatar.jpg&w=960"),
        None,
        &config,
    )
    .unwrap_err();

    assert_eq!(err, ImageError::InvalidSignature);
}

#[test]
fn protocol_less_remote_patterns_allow_http_and_https_sources() {
    let config = ImagesConfig {
        remote_patterns: vec!["cdn.example.com/uploads/**".to_string()],
        ..Default::default()
    };

    verify_public_image_request(
        "/_tako/image",
        Some("src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatar.jpg&w=960"),
        None,
        &config,
    )
    .expect("protocol-less pattern should match https");

    verify_public_image_request(
        "/_tako/image",
        Some("src=http%3A%2F%2Fcdn.example.com%2Fuploads%2Favatar.jpg&w=960"),
        None,
        &config,
    )
    .expect("protocol-less pattern should match http");
}

#[test]
fn public_image_requests_reject_unbounded_variants() {
    for query in [
        "src=%2Favatar.png&w=641",
        "src=%2Favatar.png&w=0640",
        "src=%2Favatar.png&w=640&q=80",
        "src=%2Favatar.png&w=640&q=75&q=75",
        "src=%2Favatar.png&w=640&x=1",
        "src=%2Favatar.png&w=640&f=avif",
        "src=%2Favatar.png&w=640&f=jpeg",
    ] {
        assert!(
            verify_public_image_request(
                "/_tako/image",
                Some(query),
                None,
                &ImagesConfig::default()
            )
            .is_err(),
            "expected query to fail: {query}"
        );
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
