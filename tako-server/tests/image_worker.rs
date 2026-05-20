use std::io::Cursor;
use std::io::Write;
use std::process::{Command, Stdio};

use image::{ImageBuffer, ImageFormat, Rgb};

const PNG_1X1_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=";

#[test]
fn hidden_image_worker_mode_returns_protocol_errors_on_stdout() {
    let request = serde_json::json!({
        "source_base64": "bm90LWltYWdl",
        "source_content_type": "image/png",
        "format": "avif",
        "width": 16,
        "height": null,
        "fit": null,
        "crop": null,
        "quality": 75,
        "limits": {
            "max_source_bytes": 8388608,
            "max_image_width": 8000,
            "max_image_height": 8000,
            "max_decoded_pixels": 32000000
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_tako-server"))
        .arg("--image-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn image worker");
    child
        .stdin
        .take()
        .expect("worker stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write worker request");

    let output = child.wait_with_output().expect("wait for image worker");

    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("worker JSON response"),
        serde_json::json!({
            "status": "error",
            "error": "unsupported_format"
        })
    );
}

#[test]
fn hidden_image_worker_mode_transforms_valid_sources() {
    let request = serde_json::json!({
        "source_base64": PNG_1X1_BASE64,
        "source_content_type": "image/png",
        "format": "webp",
        "width": 16,
        "height": null,
        "fit": null,
        "crop": null,
        "quality": 75,
        "limits": {
            "max_source_bytes": 8388608,
            "max_image_width": 8000,
            "max_image_height": 8000,
            "max_decoded_pixels": 32000000
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_tako-server"))
        .arg("--image-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn image worker");
    child
        .stdin
        .take()
        .expect("worker stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write worker request");

    let output = child.wait_with_output().expect("wait for image worker");
    let response =
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("worker JSON response");

    assert!(output.status.success());
    assert_eq!(response["status"], "ok");
    assert_eq!(response["format"], "webp");
    assert_eq!(response["width"], 1);
    assert_eq!(response["height"], 1);
    assert!(
        response["bytes_base64"]
            .as_str()
            .is_some_and(|value| value.starts_with("UklGR"))
    );
}

#[test]
fn hidden_image_worker_mode_transforms_large_jpeg_sources() {
    let mut source = Cursor::new(Vec::new());
    let img = ImageBuffer::from_fn(1672, 941, |x, y| {
        Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
    });
    img.write_to(&mut source, ImageFormat::Jpeg)
        .expect("encode jpeg");

    let request = serde_json::json!({
        "source_base64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, source.get_ref()),
        "source_content_type": "image/jpeg",
        "format": "webp",
        "width": 1200,
        "height": null,
        "fit": null,
        "crop": null,
        "quality": 75,
        "limits": {
            "max_source_bytes": 8388608,
            "max_image_width": 8000,
            "max_image_height": 8000,
            "max_decoded_pixels": 32000000
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_tako-server"))
        .arg("--image-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn image worker");
    child
        .stdin
        .take()
        .expect("worker stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write worker request");

    let output = child.wait_with_output().expect("wait for image worker");
    let response =
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("worker JSON response");

    assert!(
        output.status.success(),
        "worker exited with {}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(response["status"], "ok");
    assert_eq!(response["format"], "webp");
    assert_eq!(response["width"], 1200);
    assert_eq!(response["height"], 675);
    assert!(
        response["bytes_base64"]
            .as_str()
            .is_some_and(|value| value.starts_with("UklGR"))
    );
}
