pub use tako_channels::*;

use crate::instances::{INTERNAL_STATUS_HOST, INTERNAL_TOKEN_HEADER, Instance};
use crate::proxy::MAX_REQUEST_BODY_BYTES;
use crate::release::app_runtime_data_paths;
use bytes::Bytes;
use pingora_core::{Error, ErrorType};
use std::path::{Path, PathBuf};

pub(crate) fn app_channels_db_path(data_dir: &Path, app_name: &str) -> PathBuf {
    tako_channels::channels_db_path(&app_runtime_data_paths(data_dir, app_name).tako)
}

/// Read a request body with a hard size cap.
///
/// Channel POSTs run inside `request_filter`, which bypasses Pingora's
/// `request_body_filter` hook. That means the proxy-wide chunked-body cap
/// in `request_body_filter` doesn't apply here, so we enforce the same
/// limit inline to prevent unbounded memory growth from an attacker using
/// chunked transfer encoding (no Content-Length header).
pub(crate) async fn read_request_body(
    session: &mut pingora_proxy::Session,
) -> pingora_core::Result<Bytes> {
    let mut body = bytes::BytesMut::new();
    while let Some(chunk) = session.as_downstream_mut().read_request_body().await? {
        if (body.len() as u64).saturating_add(chunk.len() as u64) > MAX_REQUEST_BODY_BYTES {
            return Err(Error::explain(
                ErrorType::InvalidHTTPHeader,
                "Request body exceeds maximum allowed size",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

pub(crate) async fn authorize_channel_request(
    instance: &Instance,
    operation: ChannelOperation,
    channel: &str,
    params: serde_json::Value,
    header: Option<ChannelHeaderValue>,
    cookie: Option<String>,
) -> Result<ChannelAuthResponse, ChannelError> {
    let endpoint = instance.endpoint().ok_or(ChannelError::AuthUnavailable)?;
    tako_channels::authorize_channel_request(
        &endpoint.to_string(),
        INTERNAL_STATUS_HOST,
        INTERNAL_TOKEN_HEADER,
        instance.internal_token(),
        operation,
        channel,
        params,
        header,
        cookie,
    )
    .await
}
