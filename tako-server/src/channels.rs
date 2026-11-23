pub use tako_channels::*;

use crate::instances::{INTERNAL_TOKEN_HEADER, Instance, internal_app_host_for_app_id};

/// Without a native broker the proxy and SDK publisher use the very same
/// handle, including its memory-only notifications and presence.
#[derive(Default)]
pub(crate) struct DirectChannelStores(
    parking_lot::Mutex<std::collections::HashMap<tako_network::AppEnv, DirectStore>>,
);

struct DirectStore {
    target: ChannelTarget,
    store: std::sync::Arc<ChannelStore>,
}

impl DirectChannelStores {
    /// The app's writer for `target`. Blocking: opening reaches the database.
    pub(crate) fn open(
        &self,
        app: &tako_network::AppEnv,
        target: &ChannelTarget,
    ) -> Result<std::sync::Arc<ChannelStore>, ChannelError> {
        let mut stores = self.0.lock();
        if let Some(cached) = stores.get(app)
            && cached.target == *target
        {
            return Ok(cached.store.clone());
        }
        let store = std::sync::Arc::new(
            ChannelStore::open_config(target.config(app))
                .map_err(|_| ChannelError::Storage("cannot open channel storage".into()))?,
        );
        if let Some(old) = stores.insert(
            app.clone(),
            DirectStore {
                target: target.clone(),
                store: store.clone(),
            },
        ) {
            old.store.signal_gap(None);
        }
        Ok(store)
    }

    /// Forgets a deleted app's store. Its maintenance worker stops once the
    /// last request still holding the store finishes.
    pub(crate) fn remove(&self, app: &tako_network::AppEnv) {
        if let Some(old) = self.0.lock().remove(app) {
            old.store.signal_gap(None);
        }
    }
}

/// An app's channel replay database binding. A broker registration never
/// accepts the hosted HTTP endpoint: both processes derive it from server
/// configuration, not the IPC caller.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ChannelTarget {
    Hosted { token: zeroize::Zeroizing<String> },
}

pub(crate) type ChannelTargetResolver =
    std::sync::Arc<dyn Fn(&str) -> Result<ChannelTarget, ChannelError> + Send + Sync>;

impl ChannelTarget {
    /// The app's committed replay binding. An app none of whose deployed
    /// releases uses channels has none, and its channels are unavailable.
    pub(crate) fn for_app(
        store: &crate::state_store::SqliteStateStore,
        app: &str,
    ) -> Result<Self, ChannelError> {
        let hosted = store
            .channel_credentials(app)
            .map_err(|_| ChannelError::Storage("channel credentials unavailable".into()))?;
        Self::resolve(hosted)
    }

    fn hosted_url(token: &str) -> String {
        #[cfg(test)]
        if let Some(url) = test_replay::url(token) {
            return url;
        }
        let _ = token;
        format!("http://127.0.0.1:{}", crate::databases::app_port())
    }

    pub(crate) fn resolve(
        hosted: Option<tako_core::DatabaseConnection>,
    ) -> Result<Self, ChannelError> {
        let connection = hosted.ok_or(ChannelError::NotProvisioned)?;
        if connection.url != Self::hosted_url(&connection.auth_token) {
            return Err(ChannelError::Storage(
                "invalid hosted channel replay endpoint".into(),
            ));
        }
        let target = Self::Hosted {
            token: zeroize::Zeroizing::new(connection.auth_token),
        };
        target
            .validate()
            .map_err(|error| ChannelError::Storage(error.into()))?;
        Ok(target)
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let Self::Hosted { token } = self;
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid hosted channel replay credential");
        }
        Ok(())
    }

    pub(crate) fn config(&self, app: &tako_network::AppEnv) -> ChannelStoreConfig {
        let Self::Hosted { token } = self;
        ChannelStoreConfig::hosted(Self::hosted_url(token), token.as_str(), app.as_str())
    }
}

/// Hosted replay databases for this crate's tests. Each test serves its own
/// database host; targets naming one of its bindings reach it instead of the
/// server's fixed endpoint.
#[cfg(test)]
pub(crate) mod test_replay {
    use super::ChannelTarget;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use tako_db::testing::TestServer;

    fn urls() -> &'static Mutex<HashMap<String, String>> {
        static URLS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        URLS.get_or_init(Default::default)
    }

    pub(super) fn url(token: &str) -> Option<String> {
        urls().lock().unwrap().get(token).cloned()
    }

    /// A binding that `url` serves, but that no database there knows.
    pub(crate) fn target_at(url: &str) -> ChannelTarget {
        let token = tako_db::generate_token();
        urls().lock().unwrap().insert(token.clone(), url.into());
        ChannelTarget::Hosted {
            token: zeroize::Zeroizing::new(token),
        }
    }

    pub(crate) struct TestReplay {
        pub(crate) server: TestServer,
        pub(crate) path: std::path::PathBuf,
        token: String,
    }

    impl TestReplay {
        /// A replay database for `app`, bound as a deploy binds `@channels`.
        pub(crate) fn start(app: &str) -> Self {
            let server = TestServer::start();
            let database = server.database(&tako_core::channel_replay_grant_scope(app));
            urls()
                .lock()
                .unwrap()
                .insert(database.token.clone(), server.url.clone());
            Self {
                server,
                path: database.path,
                token: database.token,
            }
        }

        pub(crate) fn target(&self) -> ChannelTarget {
            ChannelTarget::Hosted {
                token: zeroize::Zeroizing::new(self.token.clone()),
            }
        }

        /// Direct access to the database file, as an operator's tools have.
        pub(crate) fn sql(&self) -> rusqlite::Connection {
            rusqlite::Connection::open(&self.path).unwrap()
        }
    }

    impl Drop for TestReplay {
        fn drop(&mut self) {
            urls().lock().unwrap().remove(&self.token);
        }
    }
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
    // The request carries the internal token: never follow a redirect with it.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
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
/// loaded channel registry, the channel must be defined there and its params
/// are checked like subscribe params, so a mistyped binding fails instead of
/// starting a stream nobody reads. Publishing never loads the registry: that
/// would wait on, or wake, an app instance, so an unloaded registry only gets
/// the shape check.
pub(crate) fn trusted_publication(
    registry: &registry::ChannelRegistry,
    request: tako_workflows::ChannelPublishRequest<'_>,
) -> Result<TrustedPublication, ChannelError> {
    let publication = parse_trusted_publication(&request)?;
    let Some(build) = request.build else {
        return Ok(publication);
    };
    match registry.cached(request.app, build, request.channel) {
        registry::CachedDefinition::NotLoaded => {}
        registry::CachedDefinition::Absent => {
            return Err(ChannelError::Undeclared {
                channel: request.channel.to_owned(),
                build: build.to_owned(),
            });
        }
        registry::CachedDefinition::Defined(definition) => {
            validate_publication(&definition, &publication, request.params)?;
        }
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
    use std::sync::Arc;

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
            let rejected = publish(params, Some("v1")).unwrap_err();
            assert!(matches!(rejected, ChannelError::ParamsInvalid(_)));
            assert_eq!(
                rejected.socket_code(),
                Some(tako_core::ErrorCode::ChannelParamsInvalid)
            );
        }
        // A loaded build that does not define the channel rejects it, so a
        // close cannot leave a durable marker on a channel nobody defined.
        for write in [
            tako_workflows::ChannelWrite::Publish(serde_json::json!({"type": "msg", "data": 1})),
            tako_workflows::ChannelWrite::Close { lifecycle: None },
        ] {
            let result = trusted_publication(
                &registry,
                tako_workflows::ChannelPublishRequest {
                    app: "app/production",
                    channel: "missing",
                    params: &serde_json::Value::Null,
                    build: Some("v1"),
                    write,
                },
            );
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("'missing' is not defined by build v1"),
                "{error:?}"
            );
            assert_eq!(
                error.socket_code(),
                Some(tako_core::ErrorCode::ChannelUnknown)
            );
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
        assert!(matches!(
            close("chat", serde_json::json!({"rom": "a"})),
            Err(ChannelError::ParamsInvalid(_))
        ));
        assert!(matches!(
            close("live", serde_json::json!({"room": "a"})),
            Err(ChannelError::BadRequest(_))
        ));
    }

    /// Publish and close errors reach the app's internal socket with the code
    /// an SDK branches on, through the server's registry check and store.
    #[tokio::test]
    async fn internal_socket_channel_errors_carry_their_codes() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let registry = Arc::new(registry::ChannelRegistry::new());
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
        let replay = test_replay::TestReplay::start("app/production");
        let target = replay.target();
        let store = tokio::task::spawn_blocking(move || {
            Arc::new(
                ChannelStore::open_config(target.config(&"app/production".parse().unwrap()))
                    .unwrap(),
            )
        })
        .await
        .unwrap();
        // The same composition as the server's publisher: registry check,
        // then the store, with channel errors converted to coded replies.
        let publisher: tako_workflows::ChannelPublishFn = {
            let registry = registry.clone();
            let store = store.clone();
            Arc::new(move |request| {
                let publication = trusted_publication(&registry, request)?;
                store
                    .publish_trusted(&publication)
                    .map(|message| serde_json::to_value(message).unwrap())
                    .map_err(tako_core::SocketError::from)
            })
        };
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("internal.sock");
        let handle = tako_workflows::enqueue_socket::spawn(
            &socket,
            Arc::new(|_| None),
            Some(publisher),
            None,
            None,
        )
        .unwrap();
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut send = async |command: serde_json::Value| -> serde_json::Value {
            write
                .write_all(format!("{command}\n").as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            serde_json::from_str(&line).unwrap()
        };
        let command = |kind: &str, channel: &str, params: serde_json::Value| {
            let mut command = serde_json::json!({
                "command": kind, "app": "app/production", "channel": channel,
                "params": params, "build": "v1",
            });
            if kind == "channel_publish" {
                command["payload"] = serde_json::json!({"type": "msg", "data": 1});
            }
            command
        };
        let room = serde_json::json!({"room": "a"});

        let published = send(command("channel_publish", "chat", room.clone())).await;
        assert_eq!(published["status"], "ok", "{published}");
        let undeclared = send(command("channel_publish", "missing", room.clone())).await;
        assert_eq!(undeclared["code"], "channel_unknown", "{undeclared}");
        for params in [
            serde_json::json!({"rom": "a"}),
            serde_json::json!({"room": null}),
        ] {
            let invalid = send(command("channel_publish", "chat", params)).await;
            assert_eq!(invalid["code"], "channel_params_invalid", "{invalid}");
        }
        assert_eq!(
            send(command("channel_close", "chat", room.clone())).await["status"],
            "ok"
        );
        let closed = send(command("channel_publish", "chat", room)).await;
        assert_eq!(closed["status"], "error");
        assert_eq!(closed["code"], "channel_closed", "{closed}");
        assert_eq!(
            closed["message"],
            "channel publish failed: channel stream is closed"
        );

        handle.shutdown().await;
    }

    #[test]
    fn deleted_apps_release_their_direct_channel_store() {
        let replay = test_replay::TestReplay::start("chat/production");
        let stores = DirectChannelStores::default();
        let app = crate::native_channel_client::app_scope("chat/production").unwrap();
        let store = stores.open(&app, &replay.target()).unwrap();
        assert!(Arc::ptr_eq(
            &stores.open(&app, &replay.target()).unwrap(),
            &store
        ));
        let mut gap = store.resets("room");
        assert_eq!(std::sync::Arc::strong_count(&store), 2);
        stores.remove(&app);
        // Only this caller still holds the store, and its subscribers reload.
        assert_eq!(std::sync::Arc::strong_count(&store), 1);
        assert!(stores.0.lock().is_empty());
        assert!(gap.has_changed().unwrap());
        gap.borrow_and_update();
    }

    #[test]
    fn apps_without_a_replay_binding_have_no_channels() {
        let error = ChannelTarget::resolve(None).err().unwrap();
        assert!(matches!(error, ChannelError::NotProvisioned));
        assert!(
            error
                .to_string()
                .contains("deploy a release that uses channels"),
            "{error}"
        );
        let (status, body) = tako_channels::authorize::error_response(&error);
        assert_eq!(status, 503);
        assert_eq!(body["error"], error.to_string());
        assert_eq!(
            tako_core::SocketError::from(error).code,
            Some(tako_core::ErrorCode::Unavailable)
        );
    }

    #[test]
    fn hosted_replay_owns_the_fixed_endpoint() {
        let token = tako_db::generate_token();
        let connection = tako_core::DatabaseConnection {
            url: ChannelTarget::hosted_url(&token),
            auth_token: token,
        };
        let target = ChannelTarget::resolve(Some(connection.clone())).unwrap();
        let scope = crate::native_channel_client::app_scope("chat").unwrap();
        assert_eq!(
            target.config(&scope),
            ChannelStoreConfig::hosted(&connection.url, &connection.auth_token, "chat/production")
        );
        let wire = serde_json::to_value(&target).unwrap();
        assert_eq!(wire["kind"], "hosted");
        assert!(wire.get("url").is_none());
        let mut invalid = wire;
        invalid["url"] = "http://127.0.0.1:1".into();
        assert!(serde_json::from_value::<ChannelTarget>(invalid).is_err());
        for url in [
            "http://example.com",
            "http://localhost:9845",
            "file:///home/db",
            "http://127.0.0.1:9845/other",
        ] {
            assert!(
                ChannelTarget::resolve(Some(tako_core::DatabaseConnection {
                    url: url.into(),
                    ..connection.clone()
                }))
                .is_err()
            );
        }
        assert!(
            ChannelTarget::resolve(Some(tako_core::DatabaseConnection {
                auth_token: String::new(),
                ..connection
            }))
            .is_err()
        );
    }
}
