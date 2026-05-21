mod errors;
mod framing;
mod pool;
mod process;
mod protocol;
mod stdio;

use pool::{acquire_worker_slot, run_worker_pool_request};
use protocol::{WorkerRequest, decode_worker_response};
use std::time::Duration;
use tako_images::{ImageError, TransformLimits, TransformOptions, TransformedImage};

const IMAGE_WORKER_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const IMAGE_WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const IMAGE_WORKER_STDERR_LIMIT: u64 = 4096;
const IMAGE_WORKER_FRAME_MAX_BYTES: usize = 64 * 1024 * 1024;
const IMAGE_WORKER_MIN_CONCURRENCY: usize = 1;
const IMAGE_WORKER_MAX_CONCURRENCY: usize = 2;
const IMAGE_WORKER_QUEUE_CAPACITY: usize = 32;
const IMAGE_LOG_SOURCE: &str = "images";

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

pub(crate) use stdio::run_stdio;
