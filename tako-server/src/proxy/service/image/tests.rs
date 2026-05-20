use super::*;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing_subscriber::prelude::*;

static NEXT_IMAGE_LOG_TEST_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn identifies_image_request_paths() {
    assert!(is_image_request_path("/_tako/image"));
    assert!(!is_image_request_path("/_tako/channels/chat"));
}

#[test]
fn image_errors_map_to_public_safe_status_codes() {
    assert_eq!(image_error_status(&ImageError::InvalidSignature), 403);
    assert_eq!(image_error_status(&ImageError::SourceTooLarge), 413);
    assert_eq!(image_error_status(&ImageError::UnsupportedFormat), 415);
    assert_eq!(image_error_status(&ImageError::TransformQueueFull), 503);
}

#[test]
fn transform_failure_falls_back_to_original_image_source() {
    let source = ImageSourceBytes::new(vec![1, 2, 3], Some("image/jpeg".to_string()));

    let response = image_response_body_from_transform_error(
        "demo/production",
        ImageError::TransformFailed,
        source,
        test_transform_options(),
    )
    .unwrap();

    assert_eq!(response.bytes, vec![1, 2, 3]);
    assert_eq!(response.content_type, "image/jpeg");
    assert!(!response.cacheable);
}

#[test]
fn transform_failure_fallback_accepts_content_type_parameters() {
    let source = ImageSourceBytes::new(
        vec![4, 5, 6],
        Some("image/webp; charset=binary".to_string()),
    );

    let response = image_response_body_from_transform_error(
        "demo/production",
        ImageError::TransformFailed,
        source,
        test_transform_options(),
    )
    .unwrap();

    assert_eq!(response.bytes, vec![4, 5, 6]);
    assert_eq!(response.content_type, "image/webp; charset=binary");
    assert!(!response.cacheable);
}

#[test]
fn transformed_image_responses_are_cacheable() {
    let response = ImageResponseBody::from_transformed(tako_images::TransformedImage {
        bytes: vec![7, 8, 9],
        content_type: "image/webp",
        format: tako_images::OutputFormat::Webp,
        width: 16,
        height: 8,
    });

    assert!(response.cacheable);
}

#[test]
fn transform_failure_without_image_content_type_stays_error() {
    let source = ImageSourceBytes::new(b"<html></html>".to_vec(), Some("text/html".to_string()));

    let err = image_response_body_from_transform_error(
        "demo/production",
        ImageError::TransformFailed,
        source,
        test_transform_options(),
    )
    .unwrap_err();

    assert_eq!(err, ImageError::TransformFailed);
}

#[test]
fn non_transform_image_errors_do_not_fallback() {
    let source = ImageSourceBytes::new(vec![1, 2, 3], Some("image/png".to_string()));

    let err = image_response_body_from_transform_error(
        "demo/production",
        ImageError::UnsupportedFormat,
        source,
        test_transform_options(),
    )
    .unwrap_err();

    assert_eq!(err, ImageError::UnsupportedFormat);
}

#[test]
fn transform_queue_full_does_not_fallback_to_original_image_source() {
    let source = ImageSourceBytes::new(vec![1, 2, 3], Some("image/png".to_string()));

    let err = image_response_body_from_transform_error(
        "demo/production",
        ImageError::TransformQueueFull,
        source,
        test_transform_options(),
    )
    .unwrap_err();

    assert_eq!(err, ImageError::TransformQueueFull);
}

#[tokio::test]
async fn transform_failure_fallback_writes_app_scoped_warning() {
    let dir = tempfile::tempdir().unwrap();
    let app = unique_image_log_app();
    let handle = crate::instances::logger::spawn_app_logger(&app, dir.path().to_path_buf());
    crate::instances::logger::register_app_logger(&app, handle.clone());

    let subscriber =
        tracing_subscriber::registry().with(crate::instances::logger::app_log_tracing_layer());
    tracing::subscriber::with_default(subscriber, || {
        let source = ImageSourceBytes::new(vec![1, 2, 3], Some("image/jpeg".to_string()));
        let response = image_response_body_from_transform_error(
            &app,
            ImageError::TransformFailed,
            source,
            test_transform_options(),
        )
        .unwrap();

        assert_eq!(response.bytes, vec![1, 2, 3]);
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    crate::instances::logger::unregister_app_logger(&app);
    drop(handle);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let content = std::fs::read_to_string(dir.path().join("current.log")).unwrap();
    assert!(content.contains("[server]"));
    assert!(content.contains("[images]"));
    assert!(content.contains("WARN"));
    assert!(content.contains("Image transform failed; serving original image"));
    assert!(content.contains("requested_format=Webp"));
    assert!(content.contains("width=1200"));
    assert!(content.contains("content_type=image/jpeg"));
}

#[test]
fn image_etag_changes_with_response_bytes() {
    let left = image_etag(b"one", "image/webp");
    let right = image_etag(b"two", "image/webp");

    assert_ne!(left, right);
}

#[test]
fn image_etag_changes_with_content_type() {
    let left = image_etag(b"bytes", "image/avif");
    let right = image_etag(b"bytes", "image/webp");

    assert_ne!(left, right);
}

#[test]
fn response_body_chunks_stop_at_source_limit() {
    let mut bytes = vec![0_u8; 4];

    let err = append_limited_body_chunk(&mut bytes, &[1, 2, 3], 6).unwrap_err();

    assert_eq!(err, ImageError::SourceTooLarge);
    assert_eq!(bytes.len(), 4);
}

#[test]
fn private_resolved_remote_addrs_are_rejected() {
    let private_addr = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 80));

    let err = validate_remote_resolved_addrs(&[private_addr]).unwrap_err();

    assert_eq!(err, ImageError::InvalidSource);
}

#[test]
fn public_resolved_remote_addrs_are_allowed() {
    let public_addr = SocketAddr::from((Ipv4Addr::new(93, 184, 216, 34), 80));

    validate_remote_resolved_addrs(&[public_addr]).expect("public address");
}

#[test]
fn source_cache_key_changes_with_app_name() {
    let source = ImageSource::LocalPath("/images/hero.jpg".to_string());
    let left = source_cache_key(
        "demo",
        std::path::Path::new("/opt/tako/apps/demo/production/releases/v1"),
        &source,
        "demo.example.com",
        Some("/"),
    );
    let right = source_cache_key(
        "other",
        std::path::Path::new("/opt/tako/apps/demo/production/releases/v1"),
        &source,
        "demo.example.com",
        Some("/"),
    );

    assert_ne!(left, right);
}

#[test]
fn source_cache_key_changes_with_app_root() {
    let source = ImageSource::LocalPath("/images/hero.jpg".to_string());
    let left = source_cache_key(
        "demo",
        std::path::Path::new("/opt/tako/apps/demo/production/releases/v1"),
        &source,
        "demo.example.com",
        Some("/"),
    );
    let right = source_cache_key(
        "demo",
        std::path::Path::new("/opt/tako/apps/demo/production/releases/v2"),
        &source,
        "demo.example.com",
        Some("/"),
    );

    assert_ne!(left, right);
}

#[test]
fn local_source_cache_key_changes_with_host_and_matched_route() {
    let source = ImageSource::LocalPath("/images/hero.jpg".to_string());
    let base = source_cache_key(
        "demo",
        std::path::Path::new("/opt/tako/apps/demo/production/releases/v1"),
        &source,
        "demo.example.com",
        Some("/"),
    );
    let other_host = source_cache_key(
        "demo",
        std::path::Path::new("/opt/tako/apps/demo/production/releases/v1"),
        &source,
        "assets.example.com",
        Some("/"),
    );
    let other_route = source_cache_key(
        "demo",
        std::path::Path::new("/opt/tako/apps/demo/production/releases/v1"),
        &source,
        "demo.example.com",
        Some("/nested"),
    );

    assert_ne!(base, other_host);
    assert_ne!(base, other_route);
}

fn test_transform_options() -> TransformOptions {
    TransformOptions {
        format: tako_images::OutputFormat::Webp,
        width: 1200,
        height: None,
        fit: None,
        crop: None,
        quality: 80,
    }
}

fn unique_image_log_app() -> String {
    let id = NEXT_IMAGE_LOG_TEST_ID.fetch_add(1, Ordering::Relaxed);
    format!("image-log-test-{id}")
}
