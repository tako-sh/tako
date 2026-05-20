use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tako_images::{
    ImageCrop, ImageError, ImageFit, OutputFormat, TransformLimits, TransformOptions,
    TransformedImage, transform_image,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Semaphore, SemaphorePermit, TryAcquireError};
use tokio::time::timeout;

const IMAGE_WORKER_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const IMAGE_WORKER_STDERR_LIMIT: u64 = 4096;
const IMAGE_WORKER_MIN_CONCURRENCY: usize = 2;
const IMAGE_WORKER_MAX_CONCURRENCY: usize = 4;
const IMAGE_WORKER_QUEUE_CAPACITY: usize = 128;

#[derive(Debug, Serialize, Deserialize)]
struct WorkerLimits {
    max_source_bytes: usize,
    max_image_width: u32,
    max_image_height: u32,
    max_decoded_pixels: u64,
}

impl From<&TransformLimits> for WorkerLimits {
    fn from(limits: &TransformLimits) -> Self {
        Self {
            max_source_bytes: limits.max_source_bytes,
            max_image_width: limits.max_image_width,
            max_image_height: limits.max_image_height,
            max_decoded_pixels: limits.max_decoded_pixels,
        }
    }
}

impl From<WorkerLimits> for TransformLimits {
    fn from(limits: WorkerLimits) -> Self {
        Self {
            max_source_bytes: limits.max_source_bytes,
            max_image_width: limits.max_image_width,
            max_image_height: limits.max_image_height,
            max_decoded_pixels: limits.max_decoded_pixels,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkerRequest {
    source_base64: String,
    source_content_type: Option<String>,
    format: OutputFormat,
    width: u32,
    height: Option<u32>,
    fit: Option<String>,
    crop: Option<String>,
    quality: u8,
    limits: WorkerLimits,
}

impl WorkerRequest {
    fn new(
        source: &[u8],
        source_content_type: Option<&str>,
        options: TransformOptions,
        limits: &TransformLimits,
    ) -> Self {
        Self {
            source_base64: BASE64_STANDARD.encode(source),
            source_content_type: source_content_type.map(str::to_string),
            format: options.format,
            width: options.width,
            height: options.height,
            fit: options.fit.map(fit_code).map(str::to_string),
            crop: options.crop.map(crop_code).map(str::to_string),
            quality: options.quality,
            limits: WorkerLimits::from(limits),
        }
    }

    fn transform_options(&self) -> Result<TransformOptions, ImageError> {
        Ok(TransformOptions {
            format: self.format,
            width: self.width,
            height: self.height,
            fit: self.fit.as_deref().map(parse_fit).transpose()?,
            crop: self.crop.as_deref().map(parse_crop).transpose()?,
            quality: self.quality,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WorkerResponse {
    Ok {
        bytes_base64: String,
        format: OutputFormat,
        width: u32,
        height: u32,
    },
    Error {
        error: String,
    },
}

pub(crate) async fn transform_in_worker(
    app_name: &str,
    source: &[u8],
    source_content_type: Option<&str>,
    options: TransformOptions,
    limits: &TransformLimits,
) -> Result<TransformedImage, ImageError> {
    let _permit = acquire_worker_slot().await?;
    let request = WorkerRequest::new(source, source_content_type, options, limits);
    let input = serde_json::to_vec(&request).map_err(|_| ImageError::TransformFailed)?;
    let output = run_worker_process(app_name, input).await?;
    decode_worker_response(&output, options.format)
}

async fn acquire_worker_slot() -> Result<SemaphorePermit<'static>, ImageError> {
    let slots = worker_slots();
    match slots.try_acquire() {
        Ok(permit) => return Ok(permit),
        Err(TryAcquireError::Closed) => return Err(ImageError::TransformFailed),
        Err(TryAcquireError::NoPermits) => {}
    }

    let _queue_slot = reserve_worker_queue_slot()?;
    slots
        .acquire()
        .await
        .map_err(|_| ImageError::TransformFailed)
}

fn reserve_worker_queue_slot() -> Result<WorkerQueueSlot, ImageError> {
    let queue_depth = worker_queue_depth();
    let mut current = queue_depth.load(Ordering::Acquire);
    loop {
        if current >= IMAGE_WORKER_QUEUE_CAPACITY {
            return Err(ImageError::TransformQueueFull);
        }
        match queue_depth.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(WorkerQueueSlot),
            Err(next) => current = next,
        }
    }
}

struct WorkerQueueSlot;

impl Drop for WorkerQueueSlot {
    fn drop(&mut self) {
        worker_queue_depth().fetch_sub(1, Ordering::AcqRel);
    }
}

async fn run_worker_process(app_name: &str, input: Vec<u8>) -> Result<Vec<u8>, ImageError> {
    let exe = worker_executable_path()?;
    let mut child = Command::new(exe)
        .arg("--image-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            tracing::warn!(
                app = %app_name,
                error = %error,
                "Failed to start image worker process"
            );
            ImageError::TransformFailed
        })?;
    let mut stdin = child.stdin.take().ok_or(ImageError::TransformFailed)?;
    let mut stdout = child.stdout.take().ok_or(ImageError::TransformFailed)?;
    let mut stderr = child.stderr.take().ok_or(ImageError::TransformFailed)?;

    match timeout(IMAGE_WORKER_EXECUTION_TIMEOUT, async move {
        stdin.write_all(&input).await.map_err(|error| {
            tracing::warn!(
                app = %app_name,
                error = %error,
                "Failed to write image worker request"
            );
            ImageError::TransformFailed
        })?;
        drop(stdin);

        let mut output = Vec::new();
        let mut stderr_output = Vec::new();
        let read = stdout.read_to_end(&mut output);
        let mut limited_stderr = (&mut stderr).take(IMAGE_WORKER_STDERR_LIMIT);
        let read_stderr = limited_stderr.read_to_end(&mut stderr_output);
        let wait = child.wait();
        let (read_result, stderr_result, wait_result) = tokio::join!(read, read_stderr, wait);
        read_result.map_err(|error| {
            tracing::warn!(
                app = %app_name,
                error = %error,
                "Failed to read image worker response"
            );
            ImageError::TransformFailed
        })?;
        stderr_result.map_err(|error| {
            tracing::warn!(
                app = %app_name,
                error = %error,
                "Failed to read image worker stderr"
            );
            ImageError::TransformFailed
        })?;
        let status = wait_result.map_err(|error| {
            tracing::warn!(
                app = %app_name,
                error = %error,
                "Failed to wait for image worker process"
            );
            ImageError::TransformFailed
        })?;
        if !status.success() {
            let stderr = worker_stderr_snippet(&stderr_output);
            let stderr_truncated = stderr_output.len() >= IMAGE_WORKER_STDERR_LIMIT as usize;
            tracing::warn!(
                app = %app_name,
                status = %status,
                stderr = %stderr,
                stderr_truncated,
                "Image worker process failed"
            );
            return Err(ImageError::TransformFailed);
        }
        Ok(output)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                app = %app_name,
                timeout_ms = IMAGE_WORKER_EXECUTION_TIMEOUT.as_millis() as u64,
                "Image worker process timed out"
            );
            Err(ImageError::TransformFailed)
        }
    }
}

fn worker_stderr_snippet(bytes: &[u8]) -> String {
    let snippet = String::from_utf8_lossy(bytes)
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    let snippet = snippet.trim();
    if snippet.is_empty() {
        "<empty>".to_string()
    } else {
        snippet.to_string()
    }
}

#[cfg(target_os = "linux")]
fn worker_executable_path() -> Result<std::path::PathBuf, ImageError> {
    // During upgrades, the installed tako-server path can be replaced while
    // the old process is still serving. `/proc/self/exe` spawns the currently
    // running process image instead of resolving the on-disk install path.
    Ok(std::path::PathBuf::from("/proc/self/exe"))
}

#[cfg(not(target_os = "linux"))]
fn worker_executable_path() -> Result<std::path::PathBuf, ImageError> {
    std::env::current_exe().map_err(|_| ImageError::TransformFailed)
}

pub(crate) fn run_stdio() -> Result<(), String> {
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .map_err(|error| format!("read image worker request: {error}"))?;
    let response = handle_request_bytes(&input);
    let output = serde_json::to_vec(&response)
        .map_err(|error| format!("encode image worker response: {error}"))?;
    std::io::stdout()
        .write_all(&output)
        .map_err(|error| format!("write image worker response: {error}"))
}

fn handle_request_bytes(input: &[u8]) -> WorkerResponse {
    let request = match serde_json::from_slice::<WorkerRequest>(input) {
        Ok(request) => request,
        Err(_) => {
            return WorkerResponse::Error {
                error: image_error_code(&ImageError::InvalidUrl).to_string(),
            };
        }
    };
    let source = match BASE64_STANDARD.decode(&request.source_base64) {
        Ok(source) => source,
        Err(_) => {
            return WorkerResponse::Error {
                error: image_error_code(&ImageError::InvalidSource).to_string(),
            };
        }
    };
    let options = match request.transform_options() {
        Ok(options) => options,
        Err(error) => {
            return WorkerResponse::Error {
                error: image_error_code(&error).to_string(),
            };
        }
    };
    let limits = TransformLimits::from(request.limits);

    match transform_image(
        &source,
        request.source_content_type.as_deref(),
        options,
        &limits,
    ) {
        Ok(transformed) => WorkerResponse::Ok {
            bytes_base64: BASE64_STANDARD.encode(transformed.bytes),
            format: transformed.format,
            width: transformed.width,
            height: transformed.height,
        },
        Err(error) => WorkerResponse::Error {
            error: image_error_code(&error).to_string(),
        },
    }
}

fn decode_worker_response(
    output: &[u8],
    expected_format: OutputFormat,
) -> Result<TransformedImage, ImageError> {
    match serde_json::from_slice(output).map_err(|_| ImageError::TransformFailed)? {
        WorkerResponse::Ok {
            bytes_base64,
            format,
            width,
            height,
        } => {
            if format != expected_format {
                return Err(ImageError::TransformFailed);
            }
            Ok(TransformedImage {
                bytes: BASE64_STANDARD
                    .decode(bytes_base64)
                    .map_err(|_| ImageError::TransformFailed)?,
                content_type: content_type_for_format(format),
                format,
                width,
                height,
            })
        }
        WorkerResponse::Error { error } => Err(image_error_from_code(&error)),
    }
}

fn worker_slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| Semaphore::new(worker_concurrency()))
}

fn worker_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(IMAGE_WORKER_MIN_CONCURRENCY)
        .clamp(IMAGE_WORKER_MIN_CONCURRENCY, IMAGE_WORKER_MAX_CONCURRENCY)
}

fn worker_queue_depth() -> &'static AtomicUsize {
    static DEPTH: OnceLock<AtomicUsize> = OnceLock::new();
    DEPTH.get_or_init(|| AtomicUsize::new(0))
}

fn content_type_for_format(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Avif => "image/avif",
        OutputFormat::Webp => "image/webp",
    }
}

fn fit_code(fit: ImageFit) -> &'static str {
    match fit {
        ImageFit::Cover => "cover",
        ImageFit::Contain => "contain",
    }
}

fn parse_fit(value: &str) -> Result<ImageFit, ImageError> {
    match value {
        "cover" => Ok(ImageFit::Cover),
        "contain" => Ok(ImageFit::Contain),
        _ => Err(ImageError::InvalidResize),
    }
}

fn crop_code(crop: ImageCrop) -> &'static str {
    match crop {
        ImageCrop::Center => "center",
        ImageCrop::Smart => "smart",
    }
}

fn parse_crop(value: &str) -> Result<ImageCrop, ImageError> {
    match value {
        "center" => Ok(ImageCrop::Center),
        "smart" => Ok(ImageCrop::Smart),
        _ => Err(ImageError::InvalidResize),
    }
}

fn image_error_code(error: &ImageError) -> &'static str {
    match error {
        ImageError::InvalidUrl => "invalid_url",
        ImageError::InvalidSource => "invalid_source",
        ImageError::InvalidWidth => "invalid_width",
        ImageError::InvalidHeight => "invalid_height",
        ImageError::InvalidResize => "invalid_resize",
        ImageError::InvalidQuality => "invalid_quality",
        ImageError::InvalidBrowserCacheMaxAge => "invalid_browser_cache_max_age",
        ImageError::InvalidSignature => "invalid_signature",
        ImageError::Expired => "expired",
        ImageError::SourceTooLarge => "source_too_large",
        ImageError::UnsupportedFormat => "unsupported_format",
        ImageError::ImageTooLarge => "image_too_large",
        ImageError::TransformFailed => "transform_failed",
        ImageError::TransformQueueFull => "transform_queue_full",
    }
}

fn image_error_from_code(code: &str) -> ImageError {
    match code {
        "invalid_url" => ImageError::InvalidUrl,
        "invalid_source" => ImageError::InvalidSource,
        "invalid_width" => ImageError::InvalidWidth,
        "invalid_height" => ImageError::InvalidHeight,
        "invalid_resize" => ImageError::InvalidResize,
        "invalid_quality" => ImageError::InvalidQuality,
        "invalid_browser_cache_max_age" => ImageError::InvalidBrowserCacheMaxAge,
        "invalid_signature" => ImageError::InvalidSignature,
        "expired" => ImageError::Expired,
        "source_too_large" => ImageError::SourceTooLarge,
        "unsupported_format" => ImageError::UnsupportedFormat,
        "image_too_large" => ImageError::ImageTooLarge,
        "transform_queue_full" => ImageError::TransformQueueFull,
        _ => ImageError::TransformFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    #[test]
    fn worker_error_codes_round_trip() {
        for error in [
            ImageError::InvalidUrl,
            ImageError::InvalidSource,
            ImageError::InvalidWidth,
            ImageError::InvalidHeight,
            ImageError::InvalidResize,
            ImageError::InvalidQuality,
            ImageError::InvalidBrowserCacheMaxAge,
            ImageError::InvalidSignature,
            ImageError::Expired,
            ImageError::SourceTooLarge,
            ImageError::UnsupportedFormat,
            ImageError::ImageTooLarge,
            ImageError::TransformFailed,
            ImageError::TransformQueueFull,
        ] {
            assert_eq!(image_error_from_code(image_error_code(&error)), error);
        }
    }

    #[test]
    fn worker_concurrency_scales_with_host_without_exceeding_limit() {
        let concurrency = worker_concurrency();

        assert!(
            (IMAGE_WORKER_MIN_CONCURRENCY..=IMAGE_WORKER_MAX_CONCURRENCY).contains(&concurrency)
        );
    }

    #[test]
    fn worker_stderr_snippet_is_single_line() {
        assert_eq!(worker_stderr_snippet(b""), "<empty>");
        assert_eq!(
            worker_stderr_snippet(b"first line\nsecond line\r\n"),
            "first line\\nsecond line\\r\\n"
        );
    }

    #[test]
    fn decode_worker_response_rejects_wrong_format() {
        let output = serde_json::to_vec(&WorkerResponse::Ok {
            bytes_base64: BASE64_STANDARD.encode([1, 2, 3]),
            format: OutputFormat::Webp,
            width: 16,
            height: 8,
        })
        .expect("encode worker response");

        let err = decode_worker_response(&output, OutputFormat::Avif).unwrap_err();

        assert_eq!(err, ImageError::TransformFailed);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_executable_path_uses_running_process_image_on_linux() {
        assert_eq!(
            worker_executable_path().expect("worker executable path"),
            std::path::PathBuf::from("/proc/self/exe")
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn worker_executable_path_uses_current_exe_off_linux() {
        assert_eq!(
            worker_executable_path().expect("worker executable path"),
            std::env::current_exe().expect("current exe")
        );
    }

    #[tokio::test]
    async fn worker_slot_queue_waits_until_permit_is_available() {
        let _lock = acquire_worker_slot_test_lock().await;
        let permits = acquire_all_worker_slots().await;
        let mut queued = tokio::spawn(acquire_worker_slot());

        if timeout(Duration::from_millis(25), &mut queued)
            .await
            .is_ok()
        {
            panic!("queued worker slot returned early");
        }

        drop(permits);

        let _queued_permit = timeout(Duration::from_secs(1), queued)
            .await
            .expect("queued worker slot should acquire after release")
            .expect("queued worker task")
            .expect("queued worker slot");
    }

    #[tokio::test]
    async fn worker_slot_queue_rejects_when_capacity_is_reached() {
        let _lock = acquire_worker_slot_test_lock().await;
        assert_eq!(worker_queue_depth().load(Ordering::Acquire), 0);
        let permits = acquire_all_worker_slots().await;
        let mut queued = Vec::with_capacity(IMAGE_WORKER_QUEUE_CAPACITY);
        for _ in 0..IMAGE_WORKER_QUEUE_CAPACITY {
            queued.push(tokio::spawn(acquire_worker_slot()));
        }
        wait_for_worker_queue_depth(IMAGE_WORKER_QUEUE_CAPACITY).await;

        let result = acquire_worker_slot().await;

        assert!(matches!(result, Err(ImageError::TransformQueueFull)));
        drop(permits);
        for task in queued {
            let permit = timeout(Duration::from_secs(1), task)
                .await
                .expect("queued worker slot should acquire after release")
                .expect("queued worker task")
                .expect("queued worker slot");
            drop(permit);
        }
        assert_eq!(worker_queue_depth().load(Ordering::Acquire), 0);
    }

    async fn acquire_all_worker_slots() -> Vec<SemaphorePermit<'static>> {
        let mut permits = Vec::with_capacity(worker_concurrency());
        for _ in 0..worker_concurrency() {
            permits.push(acquire_worker_slot().await.expect("worker slot"));
        }
        permits
    }

    async fn acquire_worker_slot_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    async fn wait_for_worker_queue_depth(expected: usize) {
        for _ in 0..100 {
            if worker_queue_depth().load(Ordering::Acquire) == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!(
            "worker queue depth did not reach {expected}; current={}",
            worker_queue_depth().load(Ordering::Acquire)
        );
    }
}
