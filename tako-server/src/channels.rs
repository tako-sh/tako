pub use tako_channels::*;

use crate::instances::{INTERNAL_TOKEN_HEADER, Instance, internal_app_host_for_app_id};
use crate::release::{app_runtime_data_paths, requested_deployment_identity};
use std::path::{Path, PathBuf};

pub(crate) fn app_channels_db_path(data_dir: &Path, app_name: &str) -> PathBuf {
    let (name, environment) = requested_deployment_identity(app_name);
    let deployment_id = tako_core::deployment_app_id(&name, &environment);
    tako_channels::channels_db_path(&app_runtime_data_paths(data_dir, &deployment_id).tako)
}

pub(crate) fn app_channel_store_config(data_dir: &Path, app_name: &str) -> ChannelStoreConfig {
    ChannelStoreConfig::sqlite(app_channels_db_path(data_dir, app_name))
}

pub(crate) fn app_channel_store_config_with_postgres(
    data_dir: &Path,
    app_name: &str,
    postgres_url: Option<&str>,
) -> ChannelStoreConfig {
    if let Some(url) = postgres_url {
        return ChannelStoreConfig::postgres(url, app_name);
    }
    app_channel_store_config(data_dir, app_name)
}

pub(crate) async fn authorize_channel_request(
    app_name: &str,
    instance: &Instance,
    operation: ChannelOperation,
    channel: &str,
    params: serde_json::Value,
    header: Option<ChannelHeaderValue>,
    cookie: Option<String>,
) -> Result<ChannelAuthResponse, ChannelError> {
    let endpoint = instance.endpoint().ok_or(ChannelError::AuthUnavailable)?;
    let internal_host = internal_app_host_for_app_id(app_name);
    tako_channels::authorize_channel_request(
        &endpoint.to_string(),
        &internal_host,
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
    app_name: &str,
    instance: &Instance,
) -> Result<Vec<ChannelDefinitionMeta>, ChannelError> {
    let endpoint = instance.endpoint().ok_or(ChannelError::AuthUnavailable)?;
    let internal_host = internal_app_host_for_app_id(app_name);
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
        .header("Host", internal_host)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_db_path_is_scoped_by_deployment_environment() {
        let data_dir = Path::new("/opt/tako");

        assert_eq!(
            app_channels_db_path(data_dir, "my-app/production"),
            Path::new("/opt/tako/apps/my-app/production/data/tako/channels.sqlite")
        );
        assert_eq!(
            app_channels_db_path(data_dir, "my-app/staging"),
            Path::new("/opt/tako/apps/my-app/staging/data/tako/channels.sqlite")
        );
    }

    #[test]
    fn bare_app_channel_db_path_defaults_to_production_environment() {
        assert_eq!(
            app_channels_db_path(Path::new("/opt/tako"), "my-app"),
            Path::new("/opt/tako/apps/my-app/production/data/tako/channels.sqlite")
        );
    }

    #[test]
    fn channel_store_config_uses_local_sqlite_path() {
        assert_eq!(
            app_channel_store_config(Path::new("/opt/tako"), "my-app/staging"),
            ChannelStoreConfig::Sqlite {
                path: Path::new("/opt/tako/apps/my-app/staging/data/tako/channels.sqlite")
                    .to_path_buf(),
            },
        );
    }

    #[test]
    fn channel_store_config_uses_postgres_when_url_is_set() {
        assert_eq!(
            app_channel_store_config_with_postgres(
                Path::new("/opt/tako"),
                "my-app/staging",
                Some("postgres://db")
            ),
            ChannelStoreConfig::Postgres {
                url: "postgres://db".to_string(),
                schema: POSTGRES_CHANNELS_SCHEMA.to_string(),
                app_id: "my-app/staging".to_string(),
            },
        );
    }
}
