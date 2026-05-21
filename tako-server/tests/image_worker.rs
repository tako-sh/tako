use std::io::Cursor;
use std::io::{Read, Write};
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
    write_worker_frame(
        child.stdin.as_mut().expect("worker stdin"),
        request.to_string().as_bytes(),
    );
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for image worker");

    assert!(output.status.success());
    assert_eq!(
        read_worker_frame(output.stdout.as_slice()),
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
    write_worker_frame(
        child.stdin.as_mut().expect("worker stdin"),
        request.to_string().as_bytes(),
    );
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for image worker");
    let response = read_worker_frame(output.stdout.as_slice());

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
    write_worker_frame(
        child.stdin.as_mut().expect("worker stdin"),
        request.to_string().as_bytes(),
    );
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for image worker");
    let response = read_worker_frame(output.stdout.as_slice());

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

#[test]
fn hidden_image_worker_mode_handles_multiple_framed_requests_in_one_process() {
    let invalid_request = serde_json::json!({
        "source_base64": "bm90LWltYWdl",
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
    let valid_request = serde_json::json!({
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
    let mut stdin = child.stdin.take().expect("worker stdin");
    let mut stdout = child.stdout.take().expect("worker stdout");

    write_worker_frame(&mut stdin, invalid_request.to_string().as_bytes());
    let first_response = read_worker_frame(&mut stdout);
    write_worker_frame(&mut stdin, valid_request.to_string().as_bytes());
    let second_response = read_worker_frame(&mut stdout);
    drop(stdin);
    let status = child.wait().expect("wait for image worker");

    assert!(status.success());
    assert_eq!(first_response["status"], "error");
    assert_eq!(second_response["status"], "ok");
    assert_eq!(second_response["format"], "webp");
}

fn write_worker_frame(writer: &mut impl Write, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("frame length");
    writer
        .write_all(&len.to_be_bytes())
        .expect("write frame length");
    writer.write_all(bytes).expect("write frame body");
    writer.flush().expect("flush frame");
}

fn read_worker_frame(mut reader: impl Read) -> serde_json::Value {
    let mut len = [0_u8; 4];
    reader.read_exact(&mut len).expect("read frame length");
    let mut body = vec![0_u8; u32::from_be_bytes(len) as usize];
    reader.read_exact(&mut body).expect("read frame body");
    serde_json::from_slice(&body).expect("worker JSON response")
}
