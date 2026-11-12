pub use tako_channels::*;

use crate::instances::{INTERNAL_TOKEN_HEADER, Instance, internal_app_host_for_app_id};
use crate::release::{app_runtime_data_paths, requested_deployment_identity};
use std::path::{Path, PathBuf};

/// A restored data tree has a new inode even when its configured path is unchanged.
/// Use stat, not an extra open/close of the SQLite file (POSIX locks are process-wide).
#[derive(Clone)]
pub(crate) struct LocalChannelFile {
    path: PathBuf,
    identity: (u64, u64),
}

impl LocalChannelFile {
    pub(crate) fn capture(path: PathBuf) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(&path)?;
        Ok(Self {
            path,
            identity: (metadata.dev(), metadata.ino()),
        })
    }

    pub(crate) fn is_current(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
    }
}

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

/// Bind a server-side publish to its stream. When the publisher's build has a
/// loaded channel registry, its params are checked like subscribe params, so a
/// mistyped binding fails instead of starting a stream nobody reads. Publishing
/// never loads the registry: that would wait on, or wake, an app instance.
pub(crate) fn trusted_publication(
    registry: &registry::ChannelRegistry,
    request: tako_workflows::ChannelPublishRequest<'_>,
) -> Result<TrustedPublication, ChannelError> {
    let publication = parse_trusted_publication(&request)?;
    if let Some(definition) = request
        .build
        .and_then(|build| registry.cached(request.app, build, request.channel))
    {
        validate_publication(&definition, &publication, request.params)?;
    }
    Ok(publication)
}

/// A publish appends its payload; a close appends the stream's terminal marker.
pub(crate) fn parse_trusted_publication(
    request: &tako_workflows::ChannelPublishRequest<'_>,
) -> Result<TrustedPublication, ChannelError> {
    match &request.write {
        tako_workflows::ChannelWrite::Publish(payload) => {
            TrustedPublication::parse(request.channel, request.params, payload.clone())
        }
        tako_workflows::ChannelWrite::Close { lifecycle } => {
            TrustedPublication::close(request.channel, request.params, lifecycle.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trusted_publications_check_params_against_the_publishers_loaded_build() {
        let registry = registry::ChannelRegistry::new();
        let definition: ChannelDefinitionMeta = serde_json::from_value(serde_json::json!({
            "channel": "chat",
            "paramsSchema": {
                "type": "object",
                "properties": {"room": {"type": "string"}},
                "required": ["room"],
                "additionalProperties": false
            },
            "auth": false
        }))
        .unwrap();
        registry
            .get_or_load("app/production", "v1", "chat", || async {
                Ok(vec![definition])
            })
            .await
            .unwrap();
        let publish = |params: serde_json::Value, build: Option<&str>| {
            trusted_publication(
                &registry,
                tako_workflows::ChannelPublishRequest {
                    app: "app/production",
                    channel: "chat",
                    params: &params,
                    build,
                    write: tako_workflows::ChannelWrite::Publish(
                        serde_json::json!({"type": "msg", "data": 1}),
                    ),
                },
            )
        };
        let bound = publish(serde_json::json!({"room": "a"}), Some("v1")).unwrap();
        assert_eq!(
            bound.stream,
            ChannelStream::new("chat", &serde_json::json!({"room": "a"}))
        );
        for params in [serde_json::json!({"rom": "a"}), serde_json::Value::Null] {
            assert!(matches!(
                publish(params, Some("v1")),
                Err(ChannelError::BadRequest(_))
            ));
        }
        // Without a loaded registry for the publisher's build, the shape is
        // still checked but the schema is not.
        assert!(publish(serde_json::json!({"rom": "a"}), Some("v2")).is_ok());
        assert!(publish(serde_json::json!({"rom": "a"}), None).is_ok());
        assert!(publish(serde_json::json!(["a"]), None).is_err());
    }

    #[tokio::test]
    async fn trusted_closes_are_checked_like_publishes_and_only_on_durable_channels() {
        let registry = registry::ChannelRegistry::new();
        let definition = |channel: &str, ephemeral: bool| -> ChannelDefinitionMeta {
            serde_json::from_value(serde_json::json!({
                "channel": channel,
                "paramsSchema": {
                    "type": "object",
                    "properties": {"room": {"type": "string"}},
                    "required": ["room"],
                    "additionalProperties": false
                },
                "auth": false,
                "ephemeral": ephemeral
            }))
            .unwrap()
        };
        registry
            .get_or_load("app/production", "v1", "chat", || async {
                Ok(vec![definition("chat", false), definition("live", true)])
            })
            .await
            .unwrap();
        let close = |channel: &str, params: serde_json::Value| {
            trusted_publication(
                &registry,
                tako_workflows::ChannelPublishRequest {
                    app: "app/production",
                    channel,
                    params: &params,
                    build: Some("v1"),
                    write: tako_workflows::ChannelWrite::Close { lifecycle: None },
                },
            )
        };
        let closed = close("chat", serde_json::json!({"room": "a"})).unwrap();
        assert!(closed.is_close());
        assert_eq!(
            closed.stream,
            ChannelStream::new("chat", &serde_json::json!({"room": "a"}))
        );
        for (channel, params) in [
            ("chat", serde_json::json!({"rom": "a"})),
            ("live", serde_json::json!({"room": "a"})),
        ] {
            assert!(matches!(
                close(channel, params),
                Err(ChannelError::BadRequest(_))
            ));
        }
    }

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
