use super::*;

impl ServerState {
    pub fn new(
        data_dir: PathBuf,
        cert_manager: Arc<CertManager>,
        acme_client: Option<Arc<AcmeClient>>,
        challenge_tokens: ChallengeTokens,
    ) -> Result<Self, StateStoreError> {
        let runtime = ServerRuntimeConfig::for_defaults(data_dir.clone());
        Self::new_with_runtime(
            data_dir,
            cert_manager,
            acme_client,
            challenge_tokens,
            runtime,
        )
    }

    pub fn new_with_runtime(
        data_dir: PathBuf,
        cert_manager: Arc<CertManager>,
        acme_client: Option<Arc<AcmeClient>>,
        challenge_tokens: ChallengeTokens,
        runtime: ServerRuntimeConfig,
    ) -> Result<Self, StateStoreError> {
        Self::new_with_runtime_upgrade_policy(
            data_dir,
            cert_manager,
            acme_client,
            challenge_tokens,
            runtime,
            None,
        )
    }

    pub(crate) fn new_with_runtime_for_reload(
        data_dir: PathBuf,
        cert_manager: Arc<CertManager>,
        acme_client: Option<Arc<AcmeClient>>,
        challenge_tokens: ChallengeTokens,
        runtime: ServerRuntimeConfig,
        upgrade_owner: &str,
    ) -> Result<Self, StateStoreError> {
        Self::new_with_runtime_upgrade_policy(
            data_dir,
            cert_manager,
            acme_client,
            challenge_tokens,
            runtime,
            Some(upgrade_owner),
        )
    }

    fn new_with_runtime_upgrade_policy(
        data_dir: PathBuf,
        cert_manager: Arc<CertManager>,
        acme_client: Option<Arc<AcmeClient>>,
        challenge_tokens: ChallengeTokens,
        runtime: ServerRuntimeConfig,
        expected_upgrade_owner: Option<&str>,
    ) -> Result<Self, StateStoreError> {
        let app_manager = Arc::new(AppManager::new(data_dir.clone()));
        let load_balancer = Arc::new(LoadBalancer::new(app_manager.clone()));
        let device_key = load_or_create_device_key(&data_dir.join("secret.key"))?;
        let state_store = Arc::new(SqliteStateStore::new(
            data_dir.join("state.sqlite"),
            device_key,
        ));
        state_store.init()?;
        let activity = state_store.try_drain_activity(false)?;
        let requested = state_store.drain_requested()?;
        if activity.is_none() && !requested {
            return Err(StateStoreError::InvalidData(
                "server drain completion in progress; retry startup".into(),
            ));
        }
        let drain_requested = Arc::new(AtomicBool::new(requested));
        let drain_activity = parking_lot::Mutex::new(if requested { None } else { activity });
        let proxy_requests = crate::request_drain::RequestDrain::default();
        if requested {
            proxy_requests.begin_drain();
        }
        let persisted_mode = state_store.server_mode()?;
        let persisted_owner = state_store.upgrade_lock_owner()?;
        let preserve_upgrade_mode = match expected_upgrade_owner {
            Some(expected)
                if persisted_mode == UpgradeMode::Upgrading
                    && persisted_owner.as_deref() == Some(expected) =>
            {
                true
            }
            Some(expected) => {
                return Err(StateStoreError::InvalidData(format!(
                    "upgrade reload owner '{expected}' does not match the active upgrade lock"
                )));
            }
            None => false,
        };
        if !preserve_upgrade_mode {
            if persisted_mode != UpgradeMode::Normal {
                state_store.set_server_mode(UpgradeMode::Normal)?;
            }
            if let Some(owner) = persisted_owner {
                let _ = state_store.release_upgrade_lock(&owner);
            }
        }
        let server_mode = if preserve_upgrade_mode {
            UpgradeMode::Upgrading
        } else {
            UpgradeMode::Normal
        };

        let workflows = Arc::new(crate::workflows::WorkflowManager::new(data_dir.clone()));
        {
            let state_store = state_store.clone();
            workflows.set_postgres_url_resolver(Arc::new(move |app| {
                state_store
                    .get_runtime_credentials(app)
                    .ok()
                    .and_then(|credentials| credentials.get("postgres_url").cloned())
            }));
        }

        // Server-side channel `.publish()` writes straight to the channel
        // store for the deployed app id via the shared internal socket.
        // Stores are opened lazily and cached so repeated publishes reuse
        // the same SQLite connection. The proxy shares the registry, so
        // publish params are checked against what subscribers are checked with.
        let channel_registry = Arc::new(crate::channels::registry::ChannelRegistry::new());
        {
            let native_publisher = native_channel_client(&data_dir)?
                .map(crate::native_channel_client::BlockingPublisher::new)
                .transpose()
                .map_err(|error| StateStoreError::InvalidData(error.to_string()))?;
            let data_dir = data_dir.clone();
            let state_store = state_store.clone();
            let registry = channel_registry.clone();
            let stores: parking_lot::RwLock<HashMap<String, Arc<tako_channels::ChannelStore>>> =
                parking_lot::RwLock::new(HashMap::new());
            workflows.set_channel_publisher(std::sync::Arc::new(
                move |request: tako_workflows::ChannelPublishRequest<'_>| {
                    let app = request.app;
                    let publication = crate::channels::trusted_publication(&registry, request)
                        .map_err(|error| error.to_string())?;
                    if let Some(publisher) = &native_publisher {
                        let postgres_url = state_store
                            .get_runtime_credentials(app)
                            .map_err(|_| "channel credentials unavailable".to_string())?
                            .get("postgres_url")
                            .cloned();
                        return publisher.publish(app, &publication, postgres_url);
                    }

                    let store = if let Some(existing) = stores.read().get(app) {
                        existing.clone()
                    } else {
                        let mut guard = stores.write();
                        if let Some(existing) = guard.get(app) {
                            existing.clone()
                        } else {
                            let postgres_url = state_store
                                .get_runtime_credentials(app)
                                .ok()
                                .and_then(|credentials| credentials.get("postgres_url").cloned());
                            let config = crate::channels::app_channel_store_config_with_postgres(
                                &data_dir,
                                app,
                                postgres_url.as_deref(),
                            );
                            let opened = Arc::new(
                                tako_channels::ChannelStore::open_config(config)
                                    .map_err(|e| format!("open channel store: {e}"))?,
                            );
                            guard.insert(app.to_string(), opened.clone());
                            opened
                        }
                    };
                    store
                        .publish_trusted(&publication)
                        .map(|msg| serde_json::to_value(msg).unwrap_or(serde_json::Value::Null))
                        .map_err(|e| e.to_string())
                },
            ));
        }
        workflows.set_peer_auth(std::sync::Arc::new(|app, uid| {
            crate::isolation::authorize_internal_socket_app(app, uid)
        }));
        let cold_start = Arc::new(crate::scaling::ColdStartManager::new(
            crate::scaling::ColdStartConfig::default(),
        ));
        let spawn_gate = Arc::new(tokio::sync::RwLock::new(()));
        {
            let lb = load_balancer.clone();
            let cold_start = cold_start.clone();
            let spawn_gate = spawn_gate.clone();
            let draining = drain_requested.clone();
            workflows.set_network_router(Arc::new(move |_caller, target, invocation| {
                tokio::runtime::Handle::current().block_on(async {
                    let crate::proxy::BackendResolution::Ready { backend, .. } =
                        crate::proxy::resolve_app_backend(
                            &lb,
                            &cold_start,
                            &draining,
                            &spawn_gate,
                            target,
                        )
                        .await
                    else {
                        return tako_core::Response::error("destination app unavailable");
                    };
                    crate::proxy::invoke_network_backend(backend, target, &invocation).await
                })
            }));
        }

        if tokio::runtime::Handle::try_current().is_ok() {
            workflows
                .start_socket()
                .map_err(|error| StateStoreError::InvalidData(error.to_string()))?;
        }

        Ok(Self {
            app_manager,
            load_balancer,
            cert_manager,
            acme_client: AsyncRwLock::new(acme_client),
            challenge_tokens,
            routes: Arc::new(SyncRwLock::new(RouteTable::default())),
            management_lock: Arc::new(tokio::sync::Mutex::new(())),
            spawn_gate,
            channel_credentials_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            channel_registry,
            deploy_locks: AsyncRwLock::new(HashMap::new()),
            backup_locks: SyncRwLock::new(HashMap::new()),
            database_snapshots: SyncRwLock::new(HashMap::new()),
            prepared_deploy_ssl: AsyncRwLock::new(HashMap::new()),
            cold_start,
            state_store,
            drain_requested,
            drain_activity,
            drain_observer_started: AtomicBool::new(false),
            proxy_requests,
            server_mode: AsyncRwLock::new(server_mode),
            runtime,
            workflows,
        })
    }
}

pub(crate) fn native_channel_client(
    data_dir: &std::path::Path,
) -> Result<Option<crate::native_channels::Client>, StateStoreError> {
    match std::fs::symlink_metadata(data_dir.join("native-network.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StateStoreError::InvalidData(error.to_string())),
        Ok(_) => {}
    }
    let config = tako_network::NodeConfig::load(data_dir)
        .map_err(|error| StateStoreError::InvalidData(error.to_string()))?;
    Ok(Some(crate::native_channels::Client::new(
        config.network_dir(data_dir).join("channels.sock"),
    )))
}
