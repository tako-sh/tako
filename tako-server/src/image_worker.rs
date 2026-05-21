use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tako_images::{
    ImageCrop, ImageError, ImageFit, OutputFormat, TransformLimits, TransformOptions,
    TransformedImage, transform_image,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Semaphore, SemaphorePermit, TryAcquireError};
use tokio::time::timeout;

const IMAGE_WORKER_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const IMAGE_WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const IMAGE_WORKER_STDERR_LIMIT: u64 = 4096;
const IMAGE_WORKER_FRAME_MAX_BYTES: usize = 64 * 1024 * 1024;
const IMAGE_WORKER_MIN_CONCURRENCY: usize = 1;
const IMAGE_WORKER_MAX_CONCURRENCY: usize = 2;
const IMAGE_WORKER_QUEUE_CAPACITY: usize = 32;
const IMAGE_LOG_SOURCE: &str = "images";

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
    let output = run_worker_pool_request(app_name, input).await?;
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

async fn run_worker_pool_request(app_name: &str, input: Vec<u8>) -> Result<Vec<u8>, ImageError> {
    start_worker_reaper();
    let mut worker = checkout_worker(app_name).await?;
    let result = timeout(
        IMAGE_WORKER_EXECUTION_TIMEOUT,
        worker.request(app_name, &input),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            checkin_worker(worker).await;
            Ok(output)
        }
        Ok(Err(error)) => {
            worker.stop().await;
            Err(error)
        }
        Err(_) => {
            tracing::warn!(
                app = %app_name,
                source = IMAGE_LOG_SOURCE,
                timeout_ms = IMAGE_WORKER_EXECUTION_TIMEOUT.as_millis() as u64,
                "Image worker request timed out"
            );
            worker.stop().await;
            Err(ImageError::TransformFailed)
        }
    }
}

struct ImageWorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    idle_since: Instant,
}

impl ImageWorkerProcess {
    async fn spawn(app_name: &str) -> Result<Self, ImageError> {
        let exe = worker_executable_path()?;
        let mut command = Command::new(exe);
        command
            .arg("--image-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            tracing::warn!(
                app = %app_name,
                source = IMAGE_LOG_SOURCE,
                error = %error,
                "Failed to start image worker process"
            );
            ImageError::TransformFailed
        })?;

        if let Some(stderr) = child.stderr.take() {
            drain_worker_stderr(app_name, stderr);
        }

        let stdin = child.stdin.take().ok_or(ImageError::TransformFailed)?;
        let stdout = child.stdout.take().ok_or(ImageError::TransformFailed)?;
        Ok(Self {
            child,
            stdin,
            stdout,
            idle_since: Instant::now(),
        })
    }

    fn is_running(&mut self, app_name: &str) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                tracing::warn!(
                    app = %app_name,
                    source = IMAGE_LOG_SOURCE,
                    status = %status,
                    "Image worker process exited"
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    app = %app_name,
                    source = IMAGE_LOG_SOURCE,
                    error = %error,
                    "Failed to inspect image worker process"
                );
                false
            }
        }
    }

    async fn request(&mut self, app_name: &str, input: &[u8]) -> Result<Vec<u8>, ImageError> {
        write_worker_frame_async(&mut self.stdin, input)
            .await
            .map_err(|error| {
                tracing::warn!(
                    app = %app_name,
                    source = IMAGE_LOG_SOURCE,
                    error = %error,
                    "Failed to write image worker request"
                );
                error
            })?;

        read_worker_frame_async(&mut self.stdout)
            .await
            .map_err(|error| {
                tracing::warn!(
                    app = %app_name,
                    source = IMAGE_LOG_SOURCE,
                    error = %error,
                    "Failed to read image worker response"
                );
                error
            })
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

async fn checkout_worker(app_name: &str) -> Result<ImageWorkerProcess, ImageError> {
    let mut workers = worker_pool().lock().await;
    while let Some(mut worker) = workers.pop() {
        if worker.is_running(app_name) {
            return Ok(worker);
        }
    }
    drop(workers);
    ImageWorkerProcess::spawn(app_name).await
}

async fn checkin_worker(mut worker: ImageWorkerProcess) {
    worker.idle_since = Instant::now();
    let mut workers = worker_pool().lock().await;
    if workers.len() < worker_concurrency() {
        workers.push(worker);
    } else {
        drop(workers);
        worker.stop().await;
    }
}

fn worker_pool() -> &'static Mutex<Vec<ImageWorkerProcess>> {
    static POOL: OnceLock<Mutex<Vec<ImageWorkerProcess>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Vec::with_capacity(worker_concurrency())))
}

fn start_worker_reaper() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(IMAGE_WORKER_IDLE_TIMEOUT).await;
                prune_idle_workers().await;
            }
        });
    });
}

async fn prune_idle_workers() {
    let now = Instant::now();
    let mut expired = Vec::new();
    let mut workers = worker_pool().lock().await;
    let mut index = 0;
    while index < workers.len() {
        if now.saturating_duration_since(workers[index].idle_since) >= IMAGE_WORKER_IDLE_TIMEOUT {
            expired.push(workers.swap_remove(index));
        } else {
            index += 1;
        }
    }
    drop(workers);

    for worker in expired {
        worker.stop().await;
    }
}

fn drain_worker_stderr(app_name: &str, mut stderr: tokio::process::ChildStderr) {
    let app_name = app_name.to_string();
    tokio::spawn(async move {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut stderr_truncated = false;
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => {
                    let remaining =
                        (IMAGE_WORKER_STDERR_LIMIT as usize).saturating_sub(buffer.len());
                    if remaining == 0 {
                        stderr_truncated = true;
                        continue;
                    }
                    let retained = read.min(remaining);
                    buffer.extend_from_slice(&chunk[..retained]);
                    stderr_truncated |= retained < read;
                }
                Err(error) => {
                    tracing::warn!(
                        app = %app_name,
                        source = IMAGE_LOG_SOURCE,
                        error = %error,
                        "Failed to read image worker stderr"
                    );
                    return;
                }
            }
        }

        if !buffer.is_empty() {
            let stderr = worker_stderr_snippet(&buffer);
            tracing::warn!(
                app = %app_name,
                source = IMAGE_LOG_SOURCE,
                stderr = %stderr,
                stderr_truncated,
                "Image worker wrote to stderr"
            );
        }
    });
}

async fn write_worker_frame_async(writer: &mut ChildStdin, bytes: &[u8]) -> Result<(), ImageError> {
    let len = worker_frame_len(bytes.len()).map_err(|_| ImageError::TransformFailed)?;
    writer
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    writer
        .write_all(bytes)
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    writer
        .flush()
        .await
        .map_err(|_| ImageError::TransformFailed)
}

async fn read_worker_frame_async(reader: &mut ChildStdout) -> Result<Vec<u8>, ImageError> {
    let mut len = [0_u8; 4];
    reader
        .read_exact(&mut len)
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    let len = usize::try_from(u32::from_be_bytes(len)).map_err(|_| ImageError::TransformFailed)?;
    if len > IMAGE_WORKER_FRAME_MAX_BYTES {
        return Err(ImageError::TransformFailed);
    }
    let mut output = vec![0_u8; len];
    reader
        .read_exact(&mut output)
        .await
        .map_err(|_| ImageError::TransformFailed)?;
    Ok(output)
}

fn worker_frame_len(len: usize) -> Result<u32, ()> {
    if len > IMAGE_WORKER_FRAME_MAX_BYTES {
        return Err(());
    }
    u32::try_from(len).map_err(|_| ())
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
    apply_worker_resource_policy();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    while let Some(input) = read_worker_frame_sync(&mut reader)? {
        let response = handle_request_bytes(&input);
        let output = serde_json::to_vec(&response)
            .map_err(|error| format!("encode image worker response: {error}"))?;
        write_worker_frame_sync(&mut writer, &output)?;
    }

    Ok(())
}

#[cfg(unix)]
fn apply_worker_resource_policy() {
    // Keep image CPU below the proxy and app processes under contention.
    unsafe {
        let _ = libc::nice(10);
    }
}

#[cfg(not(unix))]
fn apply_worker_resource_policy() {}

fn read_worker_frame_sync(reader: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    let mut len = [0_u8; 4];
    match read_exact_or_clean_eof(reader, &mut len)? {
        Some(()) => {}
        None => return Ok(None),
    }
    let len = usize::try_from(u32::from_be_bytes(len))
        .map_err(|_| "image worker frame length is invalid".to_string())?;
    if len > IMAGE_WORKER_FRAME_MAX_BYTES {
        return Err("image worker frame is too large".to_string());
    }
    let mut input = vec![0_u8; len];
    reader
        .read_exact(&mut input)
        .map_err(|error| format!("read image worker frame body: {error}"))?;
    Ok(Some(input))
}

fn read_exact_or_clean_eof(
    reader: &mut impl Read,
    buffer: &mut [u8],
) -> Result<Option<()>, String> {
    let mut read = 0;
    while read < buffer.len() {
        match reader.read(&mut buffer[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => return Err("image worker frame ended early".to_string()),
            Ok(n) => read += n,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("read image worker frame length: {error}")),
        }
    }
    Ok(Some(()))
}

fn write_worker_frame_sync(writer: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    let len =
        worker_frame_len(bytes.len()).map_err(|_| "image worker frame is too large".to_string())?;
    writer
        .write_all(&len.to_be_bytes())
        .map_err(|error| format!("write image worker frame length: {error}"))?;
    writer
        .write_all(bytes)
        .map_err(|error| format!("write image worker frame body: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("flush image worker frame: {error}"))
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
    let parallelism = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(IMAGE_WORKER_MIN_CONCURRENCY);
    if parallelism <= 2 {
        IMAGE_WORKER_MIN_CONCURRENCY
    } else {
        (parallelism / 4).clamp(IMAGE_WORKER_MIN_CONCURRENCY, IMAGE_WORKER_MAX_CONCURRENCY)
    }
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
