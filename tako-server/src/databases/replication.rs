use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tako_core::StorageOperation;
use tako_raft::{Node, Peer, Replica, Resource};

const CATALOG: &str = "catalog";
const PORT: u16 = tako_core::STORAGE_PEER_PORT;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct GroupConfig {
    name: Option<String>,
    key: String,
    local: u128,
    founders: BTreeMap<u128, SocketAddr>,
}

#[derive(Clone)]
struct Group {
    name: Option<String>,
    membership: Arc<tokio::sync::Mutex<()>>,
    node: Node,
    replica: Replica,
    resource: Resource,
}

#[derive(Default)]
pub(super) struct Storage {
    peer: Peer,
    groups: RwLock<HashMap<String, Group>>,
    opening: tokio::sync::Mutex<()>,
}

impl ServerState {
    pub(super) async fn start_storage_replication(&self) -> Result<(), String> {
        let Some(proxy) = self.databases.proxy.read().clone() else {
            return Ok(());
        };
        let addr = SocketAddr::new(
            proxy.address.into(),
            env_port("TAKO_STORAGE_PEER_PORT", PORT),
        );
        let peer = self.databases.raft.peer.clone();
        let task = tokio::spawn(async move {
            loop {
                match bind_reuseport(addr) {
                    Ok(listener) => {
                        let trusted = proxy.clone();
                        if let Err(error) = peer
                            .serve(listener, move |ip| trusted.is_server_address(ip))
                            .await
                        {
                            tracing::warn!(%error,"Storage peer listener stopped");
                        }
                    }
                    Err(error) => tracing::debug!(%error,"Waiting for storage interface"),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        self.databases.tasks.lock().push(task.abort_handle());
        for (id, config) in self
            .state_store
            .replication_groups()
            .map_err(|e| e.to_string())?
        {
            self.open_storage_group(&id, &config).await?;
        }
        Ok(())
    }

    fn storage_addresses(
        &self,
        ids: impl Iterator<Item = String>,
    ) -> Result<BTreeMap<u128, SocketAddr>, String> {
        let own = self
            .databases
            .server_id()
            .ok_or("missing network identity")?;
        let proxy = self
            .databases
            .proxy
            .read()
            .clone()
            .ok_or("network not ready")?;
        let port = env_port("TAKO_STORAGE_PEER_PORT", PORT);
        ids.map(|id| {
            let ip = if id == own {
                proxy.address
            } else {
                match proxy.server_address(&id) {
                    Some(IpAddr::V6(ip)) => ip,
                    _ => return Err(format!("No trusted private address for server {id}")),
                }
            };
            Ok((u128::from(ip), SocketAddr::new(ip.into(), port)))
        })
        .collect()
    }

    async fn open_storage_group(&self, id: &str, config: &GroupConfig) -> Result<Group, String> {
        let storage = &self.databases.raft;
        if let Some(group) = storage.groups.read().get(id).cloned() {
            return Ok(group);
        }
        let dir = self.runtime_config().data_dir.join("raft").join(id);
        if id != CATALOG {
            tako_core::validate_database_generation(id)?;
        }
        create_private_dir(&dir)?;
        if self
            .state_store
            .storage_initialized(id)
            .map_err(|e| e.to_string())?
            && (!dir.join("raft.sqlite").is_file() || !dir.join("replica.sqlite").is_file())
        {
            return Err(format!(
                "Persistent Raft state for {id} is missing. Recover the member from its group."
            ));
        }
        let (node, replica) = storage
            .peer
            .open(
                id,
                decode_backup_key(&config.key)?,
                config.local,
                &dir,
                tako_raft::Config {
                    heartbeat_interval: 450,
                    election_timeout_min: 1600,
                    election_timeout_max: 3000,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        let resource = Resource::new(node.clone(), replica.clone(), &dir);
        let group = Group {
            name: config.name.clone(),
            membership: Default::default(),
            node,
            replica,
            resource,
        };
        let result = async {
            if config
                .founders
                .first_key_value()
                .is_some_and(|(id, _)| *id == config.local)
                && !group
                    .node
                    .is_initialized()
                    .await
                    .map_err(|e| e.to_string())?
            {
                if !self
                    .state_store
                    .claim_storage_bootstrap(id)
                    .map_err(|e| e.to_string())?
                {
                    return Err(format!(
                        "Refusing to bootstrap {id} twice after loss of persistent Raft state"
                    ));
                }
                group
                    .node
                    .initialize(config.founders.clone())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            if let Some(name) = &config.name {
                let stored = self.hosted_database(name)?;
                if stored.generation != id {
                    return Err("resource generation changed".to_owned());
                }
                let resource = group.resource.clone();
                match stored.engine {
                    DatabaseEngine::Sqlite => {
                        let database = DatabaseId::parse(name)?;
                        if self.databases.host.is_registered(&database) {
                            self.databases
                                .host
                                .unregister(&database)
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        self.databases
                            .host
                            .register_replicated(
                                database,
                                self.databases.database_path(name),
                                resource,
                            )
                            .map_err(|e| e.to_string())?;
                    }
                    DatabaseEngine::Kv => {
                        let store = tako_kv::StoreId::parse(name)?;
                        let host = self.databases.kv.clone();
                        let path = self.databases.kv_path(name);
                        tokio::task::spawn_blocking(move || {
                            if host.is_registered(&store) {
                                host.unregister(&store)?;
                            }
                            host.register_replicated(store, path, resource)
                        })
                        .await
                        .map_err(|e| e.to_string())?
                        .map_err(|e| e.to_string())?;
                    }
                }
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = result {
            let _ = storage.peer.close(id).await;
            return Err(error);
        }
        storage.groups.write().insert(id.to_owned(), group.clone());
        if config.name.is_some() {
            let databases = Arc::downgrade(&self.databases);
            let node = group.node.clone();
            let watcher = tokio::spawn(async move {
                tako_raft::watch_leader(&node, || {
                    if let Some(databases) = databases.upgrade() {
                        databases.refresh_homes();
                    }
                })
                .await;
            });
            self.databases.tasks.lock().push(watcher.abort_handle());
        }
        Ok(group)
    }

    pub(super) async fn coordinate_storage(
        &self,
        op: StorageOperation,
    ) -> Result<serde_json::Value, String> {
        let proxy = self
            .databases
            .proxy
            .read()
            .clone()
            .ok_or("storage requires an active Tako network")?;
        let local = u128::from(proxy.address);
        let storage = &self.databases.raft;
        match op {
            StorageOperation::SetGrant {
                generation,
                scope,
                enabled,
            } => {
                let group = storage.group(&generation)?;
                let name = group
                    .name
                    .as_ref()
                    .ok_or("catalog is not an application resource")?;
                let stored = self.hosted_database(name)?;
                if stored.generation != generation {
                    return Err("resource generation changed".into());
                }
                let secret = decode_backup_key(&stored.secrets.backup_key_base64)?;
                // HMAC binds a scope to this generation's unique secret. Peers
                // persist only its digest, keeping tokens out of Raft logs.
                use hmac::{Hmac, KeyInit, Mac};
                let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&secret)
                    .map_err(|e| e.to_string())?;
                mac.update(b"tako/resource-grant/v1\0");
                mac.update(scope.as_bytes());
                let key = BASE64.encode(mac.finalize().into_bytes());
                let hash: [u8; 32] = Sha256::digest(key.as_bytes()).into();
                let enabled_hash = enabled.then_some(hash);
                let reply = tokio::time::timeout(
                    Duration::from_secs(10),
                    group.node.client_write(tako_raft::Proposal::Grant {
                        scope,
                        key_hash: enabled_hash,
                    }),
                )
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
                if reply.data != tako_raft::Applied::NoChange {
                    return Err("storage grant was not committed".into());
                }
                Ok(serde_json::json!({"key":enabled.then_some(key)}))
            }
            StorageOperation::OpenCatalog { key } => {
                let _opening = storage.opening.lock().await;
                let mut configs = self
                    .state_store
                    .replication_groups()
                    .map_err(|e| e.to_string())?;
                let config = if let Some(config) = configs.remove(CATALOG) {
                    if key.as_ref().is_some_and(|key| key != &config.key) {
                        return Err("catalog credential mismatch".into());
                    }
                    config
                } else {
                    let founder = proxy
                        .catalog_founder
                        .clone()
                        .ok_or("network has no unique catalog founder")?;
                    let secret = match key {
                        Some(key) => {
                            decode_backup_key(&key)?;
                            key
                        }
                        None if self.databases.server_id().as_deref() == Some(&founder) => {
                            let mut bytes = [0; 32];
                            getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
                            BASE64.encode(bytes)
                        }
                        None => {
                            return Err(
                                "the network founder must establish the catalog first".into()
                            );
                        }
                    };
                    let founders = self.storage_addresses(std::iter::once(founder))?;
                    let config = GroupConfig {
                        name: None,
                        key: secret,
                        local,
                        founders,
                    };
                    self.state_store
                        .insert_replication_group(CATALOG, &config)
                        .map_err(|e| e.to_string())?;
                    config
                };
                self.open_storage_group(CATALOG, &config).await?;
                Ok(serde_json::json!({"key":config.key}))
            }
            StorageOperation::OpenReplica { claim, backup_key } => {
                let _opening = storage.opening.lock().await;
                tako_core::validate_database_generation(&claim.generation)?;
                tako_core::validate_database_name(&claim.name)?;
                decode_backup_key(&backup_key)?;
                if self
                    .state_store
                    .get_database(&claim.name)
                    .map_err(|e| e.to_string())?
                    .is_none()
                {
                    self.create_database(
                        claim.name.clone(),
                        claim.engine,
                        claim.generation.clone(),
                        backup_key.clone(),
                        None,
                    )
                    .await?;
                }
                let stored = self.hosted_database(&claim.name)?;
                if stored.generation != claim.generation
                    || stored.engine != claim.engine
                    || stored.secrets.backup_key_base64 != backup_key
                {
                    return Err("resource identity mismatch".into());
                }
                let mut configs = self
                    .state_store
                    .replication_groups()
                    .map_err(|e| e.to_string())?;
                let config = if let Some(config) = configs.remove(&claim.generation) {
                    config
                } else {
                    let secret = decode_backup_key(&stored.secrets.backup_key_base64)?;
                    let mut hash = Sha256::new();
                    hash.update(b"tako/resource-raft/v1");
                    hash.update(secret);
                    let config = GroupConfig {
                        name: Some(claim.name.clone()),
                        key: BASE64.encode(hash.finalize()),
                        local,
                        founders: self.storage_addresses(claim.founders.into_iter())?,
                    };
                    self.state_store
                        .insert_replication_group(&claim.generation, &config)
                        .map_err(|e| e.to_string())?;
                    config
                };
                self.open_storage_group(&claim.generation, &config).await?;
                Ok(serde_json::json!({}))
            }
            StorageOperation::Claim { candidate } => {
                let group = storage.group(CATALOG)?;
                let result = tokio::time::timeout(
                    Duration::from_secs(10),
                    group
                        .node
                        .client_write(tako_raft::Proposal::Claim(candidate)),
                )
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
                match result.data {
                    tako_raft::Applied::Claimed(claim) => {
                        serde_json::to_value(claim).map_err(|e| e.to_string())
                    }
                    tako_raft::Applied::EngineConflict => {
                        Err("name already reserved for another storage engine".into())
                    }
                    _ => Err("invalid storage name claim".into()),
                }
            }
            StorageOperation::AddMembers { group, targets } => {
                let group = storage.group(&group)?;
                let _lock = group.membership.lock().await;
                let node = group.node;
                let targets = self.storage_addresses(targets.into_iter())?;
                tako_raft::add_members(&node, targets)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!({}))
            }
            StorageOperation::Readiness {
                group,
                target,
                barrier,
            } => {
                let (_, addr) = self
                    .storage_addresses(std::iter::once(target))?
                    .into_iter()
                    .next()
                    .ok_or("missing readiness target")?;
                let barrier = match barrier {
                    Some(index) => index,
                    None => storage
                        .peer
                        .read_barrier(&group)
                        .await
                        .map_err(|e| e.to_string())?,
                };
                let progress = storage
                    .peer
                    .progress(&group, addr)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(
                    serde_json::json!({"barrier":barrier,"applied":progress.applied,"snapshot_bytes":progress.snapshot_bytes}),
                )
            }
            StorageOperation::SeedReplica { name, generation } => {
                let stored = self.hosted_database(&name)?;
                if generation != stored.generation {
                    return Err("resource generation changed".into());
                }
                let group = storage.group(&generation)?;
                let path = match stored.engine {
                    DatabaseEngine::Sqlite => self.databases.database_path(&name),
                    DatabaseEngine::Kv => self.databases.kv_path(&name),
                };
                if group.replica.revision().await.map_err(|e| e.to_string())? == 0 {
                    let dir = self
                        .runtime_config()
                        .data_dir
                        .join("raft")
                        .join(&generation);
                    let mut writer = tako_raft::Writer::checkout(group.node, group.replica, &dir)
                        .await
                        .map_err(|e| e.to_string())?;
                    tokio::task::spawn_blocking(move || writer.seed(&path))
                        .await
                        .map_err(|e| e.to_string())?
                        .map_err(|e| e.to_string())?;
                }
                Ok(serde_json::json!({}))
            }
            StorageOperation::Progress { group } => {
                let group = storage.group(&group)?;
                let leader = group
                    .node
                    .current_leader()
                    .await
                    .map(std::net::Ipv6Addr::from);
                Ok(
                    serde_json::json!({"leader":leader,"here":proxy.address,"revision":group.replica.revision().await.map_err(|e|e.to_string())?}),
                )
            }
        }
    }
}

impl Storage {
    pub(super) fn resource(&self, id: &str) -> Option<Resource> {
        self.groups.read().get(id).map(|g| g.resource.clone())
    }
    pub(super) fn hosts(&self, id: &str, name: &str) -> bool {
        self.groups
            .read()
            .get(id)
            .is_some_and(|group| group.name.as_deref() == Some(name))
    }
    pub(super) fn leader(&self, id: &str) -> Option<std::net::Ipv6Addr> {
        let groups = self.groups.read();
        groups
            .get(id)?
            .resource
            .current_leader()
            .map(std::net::Ipv6Addr::from)
    }

    pub(super) async fn shutdown(&self) {
        let groups = std::mem::take(&mut *self.groups.write());
        for (id, _) in groups {
            if let Err(error) = self.peer.close(&id).await {
                tracing::warn!(%error, %id, "Could not close storage group");
            }
        }
    }
    fn group(&self, id: &str) -> Result<Group, String> {
        self.groups
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| "storage group is not open here".to_owned())
    }
}
