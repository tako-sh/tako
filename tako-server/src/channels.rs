pub use tako_channels::*;

use crate::instances::{INTERNAL_STATUS_HOST, INTERNAL_TOKEN_HEADER, Instance};
use crate::release::app_runtime_data_paths;
use std::path::{Path, PathBuf};

pub(crate) fn app_channels_db_path(data_dir: &Path, app_name: &str) -> PathBuf {
    tako_channels::channels_db_path(&app_runtime_data_paths(data_dir, app_name).tako)
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

pub(crate) async fn fetch_channel_registry(
    instance: &Instance,
) -> Result<Vec<ChannelDefinitionMeta>, ChannelError> {
    let endpoint = instance.endpoint().ok_or(ChannelError::AuthUnavailable)?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| ChannelError::Storage(format!("build registry client: {e}")))?;

    let response = client
        .get(format!(
            "http://{}{}",
            endpoint,
            tako_channels::INTERNAL_CHANNEL_REGISTRY_PATH
        ))
        .header("Host", INTERNAL_STATUS_HOST)
        .header(INTERNAL_TOKEN_HEADER, instance.internal_token())
        .send()
        .await
        .map_err(|_| ChannelError::AuthUnavailable)?;

    match response.status().as_u16() {
        200 => response
            .json::<Vec<ChannelDefinitionMeta>>()
            .await
            .map_err(|e| ChannelError::BadRequest(format!("invalid channel registry: {e}"))),
        404 => Err(ChannelError::NotDefined),
        401 | 403 => Err(ChannelError::AuthUnavailable),
        status => Err(ChannelError::BadRequest(format!(
            "channel registry returned {status}"
        ))),
    }
}
