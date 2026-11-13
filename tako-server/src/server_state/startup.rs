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
        // Before anything that may start app instances.
        let standby_lock = if runtime.standby {
            Some(standby_lock_at_start(&data_dir)?)
        } else {
            None
        };
        let app_manager = Arc::new(AppManager::new(data_dir.clone()));
        let load_balancer = Arc::new(LoadBalancer::new(app_manager.clone()));
        let device_key = load_or_create_device_key(&data_dir.join("secret.key"))?;
        let state_store = Arc::new(SqliteStateStore::new(
            data_dir.join("state.sqlite"),
            device_key,
        ));
        state_store.init()?;
        let (activity, requested) = drain_activity_at_start(&state_store)?;
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
        // A companion restart during a primary upgrade must not release the
        // primary's persisted upgrade lock. Only a primary start owns recovery
        // of that state; a standby has its own local mode until promotion.
        if !preserve_upgrade_mode && !runtime.standby {
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
                    let publication = crate::channels::trusted_publication(&registry, request)?;
                    if let Some(publisher) = &native_publisher {
                        let postgres_url = state_store
                            .get_runtime_credentials(app)
                            .map_err(|_| {
                                tako_core::SocketError::new(
                                    tako_core::ErrorCode::Unavailable,
                                    "channel credentials unavailable",
                                )
                            })?
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
                        .map_err(tako_core::SocketError::from)
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
                        return tako_core::Response::coded_error(
                            tako_core::ErrorCode::Unavailable,
                            "destination app unavailable",
                        );
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
            standby_lock: parking_lot::Mutex::new(standby_lock),
            drain_observer_started: AtomicBool::new(false),
            proxy_requests,
            server_mode: AsyncRwLock::new(server_mode),
            runtime,
            workflows,
        })
    }
}

/// How long a start waits for a lease another server process holds
/// exclusively: a restore's standby lock, or the drain activity lease of a
/// normal start that recovers backup restores.
const STARTUP_LEASE_WAIT: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(5 * 60)
};

/// A standby runs the apps of the data directory, so it never starts while
/// a restore holds the standby lock: it waits for the restore to end, up to
/// [`STARTUP_LEASE_WAIT`], then fails its start.
fn standby_lock_at_start(
    data_dir: &std::path::Path,
) -> Result<crate::state_store::DrainLease, StateStoreError> {
    crate::state_store::wait_for_lease(
        STARTUP_LEASE_WAIT,
        "a backup restore on this data directory to finish before this standby runs apps",
        || crate::state_store::DrainLease::try_standby_in(data_dir, false),
    )?
    .ok_or_else(|| {
        StateStoreError::InvalidData(format!(
            "a backup restore on this data directory still runs after {} s, and a standby never runs apps during a restore; retry startup once the restore has finished",
            STARTUP_LEASE_WAIT.as_secs()
        ))
    })
}

/// The shared drain activity lease, and whether a drain is requested, in
/// which case this start takes no lease. A normal start holds the lease
/// exclusively while it recovers backup restores and sweeps work
/// directories, which can take minutes, and a drain holds it briefly when
/// it finishes: wait for either, up to [`STARTUP_LEASE_WAIT`].
fn drain_activity_at_start(
    state_store: &SqliteStateStore,
) -> Result<(Option<crate::state_store::DrainLease>, bool), StateStoreError> {
    crate::state_store::wait_for_lease(
        STARTUP_LEASE_WAIT,
        "another server process to finish recovering this data directory",
        || {
            let activity = state_store.try_drain_activity(false)?;
            let requested = state_store.drain_requested()?;
            Ok((activity.is_some() || requested).then_some((activity, requested)))
        },
    )?
    .ok_or_else(|| {
        StateStoreError::InvalidData(format!(
            "another server process held the drain activity lease exclusively for {} s (a normal start recovering backup restores on this data directory, or a server drain finishing); retry startup",
            STARTUP_LEASE_WAIT.as_secs()
        ))
    })
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
