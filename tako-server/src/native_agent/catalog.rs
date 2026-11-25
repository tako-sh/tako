//! This process owns the network catalog across app-server reloads. Only
//! enrolled server WireGuard addresses may use its Raft transport.
use super::{AgentError, Result, now};
use parking_lot::RwLock;
use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tako_network::{
    Ack, Member, MemberKind, NodeConfig, RegistrySnapshot, Revocation,
    api::{RuntimeError, RuntimeResult},
    catalog::{MAX_REGISTRY_BYTES, MAX_REGISTRY_ROWS, RegistryStore},
    node_config::protected::Directory,
    services::ServiceLease,
    shared::{SharedRecord, SharedSnapshot},
};
use tako_raft::{Applied, Node, Peer, Proposal, Replica};
use tokio::runtime::Handle;

const GROUP: &str = "catalog";
const PORT: u16 = 9851;
const FRESH: Duration = Duration::from_secs(10);

pub(super) struct Catalog {
    rt: Handle,
    node: Node,
    peer: Peer,
    replica: Replica,
    config: NodeConfig,
    peers: Arc<RwLock<BTreeMap<IpAddr, [u8; 32]>>>,
    confirmed: Option<Instant>,
    request_registry: Option<Vec<u8>>,
    membership: tokio::task::JoinHandle<()>,
    listener: tokio::task::JoinHandle<()>,
    local_lease: Option<ServiceLease>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Registry {
    current: RegistrySnapshot,
    pending: Option<Revocation>,
}

// Read the committed desired membership on every pass. A new leader can finish
// a removal even if the requesting process died after recording the intent.
async fn reconcile_membership(node: &Node, replica: &Replica, local: u128) -> io::Result<()> {
    if node.current_leader().await != Some(local) {
        return Ok(());
    }
    let (expected, bytes) = replica.catalog_document(false).await?;
    if bytes.is_empty() {
        return Ok(());
    }
    let mut registry: Registry = serde_json::from_slice(&bytes)?;
    let pending = registry.pending.take();
    if let Some(denial) = &pending {
        registry.current.revocations.push(denial.clone());
    }
    let targets =
        servers(&registry.current, now().map_err(io::Error::other)?).map_err(io::Error::other)?;
    if pending.is_none() {
        return tako_raft::reconcile(node, &targets).await;
    }
    tako_raft::reconcile_voters(node, &targets).await?;
    node.client_write(Proposal::CatalogUpdate {
        shared: false,
        expected,
        value: serde_json::to_vec(&registry)?,
    })
    .await
    .map_err(io::Error::other)?;
    Ok(())
}

fn servers(snapshot: &RegistrySnapshot, time: u64) -> Result<BTreeMap<u128, SocketAddr>> {
    Ok(snapshot
        .effective_members(time)?
        .values()
        .filter(|m| m.permissions.is_full() && matches!(m.kind, MemberKind::Server(_)))
        .map(|m| {
            let ip = snapshot.network.id.address(&m.request.wg_public_key);
            (u128::from(ip), SocketAddr::new(ip.into(), PORT))
        })
        .collect())
}

fn peer_keys(
    keys: &tako_network::LocalDeviceKeys,
    snapshot: &RegistrySnapshot,
    time: u64,
) -> Result<BTreeMap<IpAddr, [u8; 32]>> {
    snapshot
        .effective_members(time)?
        .values()
        .filter(|m| m.permissions.is_full() && matches!(m.kind, MemberKind::Server(_)))
        .map(|m| {
            Ok((
                snapshot.network.id.address(&m.request.wg_public_key).into(),
                keys.catalog_peer_key(&snapshot.network.id, &m.request.encryption_public_key)?,
            ))
        })
        .collect()
}
impl Catalog {
    pub(super) fn start(
        config: &NodeConfig,
        known: &RegistrySnapshot,
        keys: &tako_network::LocalDeviceKeys,
        root: &Path,
        files: &Directory,
        stop: &AtomicBool,
        bind: impl FnOnce(SocketAddr) -> io::Result<std::net::TcpListener>,
    ) -> Result<Self> {
        let rt = Handle::current();
        let peers = Arc::new(RwLock::new(peer_keys(keys, known, now()?)?));
        let local = known
            .members
            .iter()
            .find(|m| m.device_id == config.server_id)
            .ok_or(AgentError::Invalid("missing catalog member"))?;
        let ip = config.network.id.address(&local.request.wg_public_key);
        let address = SocketAddr::new(ip.into(), PORT);
        let founder = config
            .bootstrap
            .members
            .iter()
            .filter(|m| m.independent_root && matches!(m.kind, MemberKind::Server(_)))
            .collect::<Vec<_>>();
        if founder.len() != 1 {
            return Err(AgentError::Invalid("expected one network founder"));
        }
        let founder = founder[0].device_id == config.server_id;
        let path = root.join("catalog");
        if !path.try_exists()? {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        }
        let directory = Directory::open(&path, true)?;
        let initialized =
            files.exists("catalog-initialized")? || files.exists("bootstrap-complete")?;
        for filename in ["raft.redb", "replica.redb"] {
            if !directory.exists(filename)? {
                if initialized {
                    return Err(AgentError::Invalid("network catalog files are missing"));
                }
                directory.write(filename, b"")?;
            }
        }
        let auth = peers.clone();
        let peer = Peer::with_peer_keys(move |ip| auth.read().get(&ip).copied());
        let (node, replica) = rt.block_on(peer.open(
            GROUP,
            [0; 32],
            u128::from(ip),
            &path,
            tako_raft::Config::default(),
        ))?;
        let trusted = peers.clone();
        let transport = peer.clone();
        let socket = bind(address)?;
        socket.set_nonblocking(true)?;
        let socket = tokio::net::TcpListener::from_std(socket)?;
        let listener = rt.spawn(async move {
            if let Err(error) = transport
                .serve(socket, move |ip| trusted.read().contains_key(&ip))
                .await
            {
                tracing::error!(%error,"Network catalog listener stopped");
            }
        });
        let owner = node.clone();
        let state = replica.clone();
        let membership = rt.spawn(async move {
            loop {
                let _ = tokio::time::timeout(
                    Duration::from_secs(15),
                    reconcile_membership(&owner, &state, u128::from(ip)),
                )
                .await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        let mut catalog = Self {
            rt,
            node,
            replica,
            peer,
            config: config.clone(),
            peers,
            listener,
            membership,
            confirmed: None,
            request_registry: None,
            local_lease: None,
        };
        if founder
            && !catalog
                .rt
                .block_on(catalog.node.is_initialized())
                .map_err(io::Error::other)?
        {
            if initialized {
                return Err(AgentError::Invalid(
                    "refusing to bootstrap a replacement catalog",
                ));
            }
            catalog
                .rt
                .block_on(
                    catalog
                        .node
                        .initialize(BTreeMap::from([(u128::from(ip), address)])),
                )
                .map_err(io::Error::other)?;
        }
        if !initialized {
            files.write("catalog-initialized", b"1")?;
        }
        loop {
            if stop.load(Ordering::Acquire) {
                return Err(AgentError::Invalid("agent stopped"));
            }
            if catalog.confirm().is_ok() {
                let (version, bytes) = catalog.document(false)?;
                if bytes.is_empty() && founder && version == 0 {
                    catalog.update(
                        false,
                        version,
                        &Registry {
                            current: config.bootstrap.clone(),
                            pending: None,
                        },
                    )?;
                }
                if !catalog.document(false)?.1.is_empty() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(catalog)
    }

    // Fix the authorization view for one signed operation. A membership change
    // between verification and proposal construction must cause a retry. The
    // revision on the proposal closes the race between construction and commit.
    pub(super) fn begin_request(&mut self) -> RuntimeResult<()> {
        self.request_registry = None;
        self.confirmed = None;
        self.confirm()?;
        self.request_registry = Some(self.document(false)?.1);
        Ok(())
    }
    pub(super) fn end_request(&mut self) {
        self.request_registry = None;
    }
    fn document(&self, shared: bool) -> RuntimeResult<(u64, Vec<u8>)> {
        let read = |kind| {
            self.rt
                .block_on(self.replica.catalog_document(kind))
                .map_err(|_| RuntimeError::Transport)
        };
        let (revision, bytes) = read(shared)?;
        if let Some(authorized) = &self.request_registry {
            let matches = if shared {
                let (version, members) = read(false)?;
                version == revision && &members == authorized
            } else {
                &bytes == authorized
            };
            if !matches {
                return Err(RuntimeError::Conflict("network authority changed"));
            }
        }
        Ok((revision, bytes))
    }
    fn update(
        &mut self,
        shared: bool,
        expected: u64,
        new: &impl serde::Serialize,
    ) -> RuntimeResult<()> {
        let value = serde_json::to_vec(new)?;
        let own_registry = (!shared && self.request_registry.is_some()).then(|| value.clone());
        let proposal = Proposal::CatalogUpdate {
            shared,
            expected,
            value,
        };
        match self
            .rt
            .block_on(self.peer.catalog_write(proposal))
            .map_err(|_| RuntimeError::Transport)?
        {
            Applied::Committed(_) => {
                if let Some(registry) = own_registry {
                    self.request_registry = Some(registry);
                }
                Ok(())
            }
            Applied::Stale { .. } => Err(RuntimeError::Conflict("catalog changed; retry")),
            _ => Err(RuntimeError::Invalid("catalog update")),
        }
    }
    fn confirm(&mut self) -> RuntimeResult<()> {
        let start = Instant::now();
        if self
            .confirmed
            .is_some_and(|at| start.duration_since(at) < FRESH)
        {
            return Ok(());
        }
        self.rt
            .block_on(tokio::time::timeout(Duration::from_secs(5), async {
                let pos = self.peer.committed_position(GROUP).await?;
                loop {
                    if self
                        .peer
                        .local_progress(GROUP)?
                        .applied
                        .is_some_and(|n| n >= pos)
                    {
                        return Ok::<_, io::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }))
            .map_err(|_| RuntimeError::Transport)?
            .map_err(|_| RuntimeError::Transport)?;
        self.confirmed = Some(start);
        Ok(())
    }
    pub(super) fn withdraw(&mut self) {
        self.peers.write().clear();
        self.confirmed = None;
        self.local_lease = None;
    }
    pub(super) fn reconcile(
        &mut self,
        keys: &tako_network::LocalDeviceKeys,
        snapshot: &RegistrySnapshot,
        time: u64,
    ) -> Result<()> {
        *self.peers.write() = peer_keys(keys, snapshot, time)?;
        Ok(())
    }
    fn change_registry(
        &mut self,
        change: impl FnOnce(&mut RegistrySnapshot) -> RuntimeResult<()>,
    ) -> RuntimeResult<()> {
        self.confirm()?;
        let (version, bytes) = self.document(false)?;
        let mut document: Registry = serde_json::from_slice(&bytes)?;
        if document.pending.is_some() {
            return Err(RuntimeError::Conflict("server removal in progress"));
        }
        let registry = &mut document.current;
        change(registry)?;
        registry.validate()?;
        if registry.members.len() + registry.revocations.len() + registry.acks.len() + 1
            > MAX_REGISTRY_ROWS
            || serde_json::to_vec(&registry)?.len() > MAX_REGISTRY_BYTES
        {
            return Err(RuntimeError::Capacity("registry projection"));
        }
        self.update(false, version, &document)
    }
    pub(super) fn publish_services(&mut self, lease: &ServiceLease) -> Result<()> {
        self.peer.publish_announcement(serde_json::to_vec(lease)?)?;
        self.local_lease = Some(lease.clone());
        Ok(())
    }
    pub(super) fn service_leases(
        &self,
        registry: &RegistrySnapshot,
        time: u64,
    ) -> Result<Vec<ServiceLease>> {
        use futures_util::{StreamExt, stream};
        let effective = registry.effective_members(time)?;
        let others = effective.values().filter(|m| {
            m.device_id != self.config.server_id
                && matches!(m.kind, MemberKind::Server(_))
                && m.permissions.is_full()
        });
        let mut leases = self.rt.block_on(async {
            stream::iter(others.map(|member| async move {
                let address = registry.network.id.address(&member.request.wg_public_key);
                let bytes = tokio::time::timeout(
                    Duration::from_millis(750),
                    self.peer
                        .announcement(SocketAddr::new(address.into(), PORT)),
                )
                .await
                .ok()?
                .ok()?;
                let lease = ServiceLease::from_slice(&bytes).ok()?;
                lease
                    .verify_member(&registry.network.id, member, time)
                    .ok()?;
                Some(lease)
            }))
            .buffer_unordered(64)
            .filter_map(|lease| async { lease })
            .collect::<Vec<_>>()
            .await
        });
        leases.extend(self.local_lease.clone());
        Ok(leases)
    }
    fn publication(&mut self, candidate: tako_core::StorageClaim) -> RuntimeResult<Proposal> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        use tako_network::shared::{
            DATABASE_SCOPE, RecordAddress, RecordContent, RecordKind, RecordMutation,
        };
        use zeroize::Zeroizing;
        // Both documents must describe the same committed membership. A change
        // after encryption invalidates the proposal at apply time.
        let (expected, bytes) = self.document(true)?;
        let (members_revision, members) = self.document(false)?;
        if members_revision != expected {
            return Err(RuntimeError::Conflict("catalog changed; retry"));
        }
        let members = serde_json::from_slice::<Registry>(&members)?.current;
        let network = members.network.id.clone();
        let mut shared = if bytes.is_empty() {
            SharedSnapshot::empty(network.clone())
        } else {
            serde_json::from_slice(&bytes)?
        };
        let mut record = serde_json::to_value(&candidate)?;
        record["server"] = serde_json::json!(
            candidate
                .founders
                .first()
                .ok_or(RuntimeError::Invalid("missing resource founder"))?
        );
        let mut key = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut *key).map_err(|_| RuntimeError::Invalid("backup key generation"))?;
        let time = now().map_err(|_| RuntimeError::Invalid("clock"))?;
        let entries = [
            (
                RecordKind::Database,
                format!("live.{}", candidate.generation),
                Zeroizing::new(serde_json::to_string(&record)?),
            ),
            (
                RecordKind::DatabaseKey,
                format!("{}.{}", candidate.name, candidate.generation),
                Zeroizing::new(STANDARD.encode(&*key)),
            ),
        ];
        for (kind, name, value) in entries {
            let mutation = RecordMutation {
                id: tako_network::RequestId::random()?,
                observed_revision: 0,
                expires_at: time,
                address: RecordAddress {
                    scope: DATABASE_SCOPE.parse()?,
                    kind,
                    name,
                },
                value: None,
            };
            let bytes = Zeroizing::new(serde_json::to_vec(&RecordContent {
                value,
                expires_on: None,
            })?);
            let published = SharedRecord::create(
                &mutation,
                Some(&bytes),
                1,
                self.config.server_id.clone(),
                self.config.server_id.clone(),
                &members,
                time,
            )?;
            shared.merge(&SharedSnapshot {
                network: network.clone(),
                records: vec![published],
            })?;
        }
        Ok(Proposal::PublishClaim {
            candidate,
            expected,
            shared: serde_json::to_vec(&shared)?,
        })
    }
    fn retire_name(&self, name: String, generation: String) -> RuntimeResult<Proposal> {
        use tako_network::shared::{DATABASE_SCOPE, RecordAddress, RecordKind, RecordMutation};
        let (expected, bytes) = self.document(true)?;
        let mut shared: SharedSnapshot = serde_json::from_slice(&bytes)?;
        let address = RecordAddress {
            scope: DATABASE_SCOPE.parse()?,
            kind: RecordKind::Database,
            name: format!("live.{generation}"),
        };
        let current = shared
            .latest()?
            .get(&address)
            .copied()
            .ok_or(RuntimeError::Invalid("missing catalog directory entry"))?;
        if let Some(plaintext) = &current.plaintext {
            let record: serde_json::Value = serde_json::from_str(&plaintext.value)?;
            if record["name"] != name || record["generation"] != generation {
                return Err(RuntimeError::Invalid("retirement identity"));
            }
            let (version, bytes) = self.document(false)?;
            if version != expected {
                return Err(RuntimeError::Conflict("catalog changed; retry"));
            }
            let members = serde_json::from_slice::<Registry>(&bytes)?.current;
            let time = now().map_err(|_| RuntimeError::Invalid("clock"))?;
            let mutation = RecordMutation {
                id: tako_network::RequestId::random()?,
                observed_revision: current.revision,
                expires_at: time,
                address,
                value: None,
            };
            let revision = current
                .revision
                .checked_add(1)
                .ok_or(RuntimeError::Invalid("catalog revision overflow"))?;
            let record = SharedRecord::create(
                &mutation,
                None,
                revision,
                self.config.server_id.clone(),
                self.config.server_id.clone(),
                &members,
                time,
            )?;
            shared.merge(&SharedSnapshot {
                network: members.network.id,
                records: vec![record],
            })?;
        }
        Ok(Proposal::RetireName {
            name,
            generation,
            expected,
            shared: serde_json::to_vec(&shared)?,
        })
    }
    pub(super) fn storage(
        &mut self,
        command: tako_core::StorageOperation,
    ) -> RuntimeResult<serde_json::Value> {
        use tako_core::StorageOperation as Op;
        self.confirm()?;
        let registry = serde_json::from_slice::<Registry>(&self.document(false)?.1)?.current;
        if !registry
            .effective_members(now().map_err(|_| RuntimeError::Invalid("clock"))?)?
            .contains_key(&self.config.server_id)
        {
            return Err(RuntimeError::Denied);
        }
        let proposal = match command {
            Op::Claim { candidate } => self.publication(candidate)?,
            Op::CatalogStatus => {
                self.confirmed = None;
                self.confirm()?;
                return Ok(serde_json::json!({}));
            }
            Op::RetireName { name, generation } => self.retire_name(name, generation)?,
            Op::Retirement {
                generation,
                targets,
            } => {
                if let Some(targets) = targets {
                    if self
                        .rt
                        .block_on(self.peer.catalog_write(Proposal::Retirement {
                            generation: generation.clone(),
                            targets,
                        }))
                        .map_err(|_| RuntimeError::Transport)?
                        != Applied::NoChange
                    {
                        return Err(RuntimeError::Conflict("retirement"));
                    }
                } else {
                    self.confirmed = None;
                    self.confirm()?;
                }
                return Ok(serde_json::to_value(
                    self.rt
                        .block_on(self.replica.retirement(generation))
                        .map_err(|_| RuntimeError::Transport)?,
                )?);
            }
            _ => return Err(RuntimeError::Invalid("catalog operation")),
        };
        match self
            .rt
            .block_on(self.peer.catalog_write(proposal))
            .map_err(|_| RuntimeError::Transport)?
        {
            Applied::Claimed(c) => Ok(serde_json::to_value(c)?),
            Applied::NoChange => Ok(serde_json::json!({})),
            _ => Err(RuntimeError::Conflict("resource name or generation")),
        }
    }
}
impl Drop for Catalog {
    fn drop(&mut self) {
        self.listener.abort();
        self.membership.abort();
        let _ = self.rt.block_on(self.peer.close(GROUP));
    }
}
impl RegistryStore for Catalog {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        self.confirm()?;
        let registry = serde_json::from_slice::<Registry>(&self.document(false)?.1)?.current;
        if registry.network != self.config.network {
            return Err(RuntimeError::Conflict("network identity"));
        }
        Ok(registry)
    }
    fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
        self.change_registry(|registry| {
            match registry
                .members
                .iter()
                .find(|m| m.device_id == member.device_id)
            {
                Some(existing) if existing != member => {
                    return Err(RuntimeError::Conflict("member"));
                }
                None => registry.members.push(member.clone()),
                _ => {}
            }
            if registry.members.len() + registry.revocations.len() + 1 > MAX_REGISTRY_ROWS / 2
                || serde_json::to_vec(&registry)?.len() > MAX_REGISTRY_BYTES / 2
            {
                return Err(RuntimeError::Capacity("registry admissions"));
            }
            Ok(())
        })
    }
    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
        self.confirm()?;
        let (version, bytes) = self.document(false)?;
        let mut doc: Registry = serde_json::from_slice(&bytes)?;
        if let Some(existing) = doc
            .current
            .revocations
            .iter()
            .find(|r| r.id == revocation.id)
        {
            return if existing == revocation {
                Ok(())
            } else {
                Err(RuntimeError::Conflict("revocation"))
            };
        }
        let mut after = doc.current.clone();
        after.revocations.push(revocation.clone());
        after.validate()?;
        if after.members.len() + after.revocations.len() + after.acks.len() + 1 > MAX_REGISTRY_ROWS
            || serde_json::to_vec(&after)?.len() > MAX_REGISTRY_BYTES
        {
            return Err(RuntimeError::Capacity("registry projection"));
        }
        let time = now().map_err(|_| RuntimeError::Invalid("clock"))?;
        let targets = servers(&after, time).map_err(|_| RuntimeError::Invalid("registry"))?;
        if let Some(pending) = &doc.pending {
            if pending != revocation {
                return Err(RuntimeError::Conflict("another server removal in progress"));
            }
        } else if targets.is_empty()
            || targets
                == servers(&doc.current, time).map_err(|_| RuntimeError::Invalid("registry"))?
        {
            doc.current = after;
            return self.update(false, version, &doc);
        } else {
            doc.pending = Some(revocation.clone());
            self.update(false, version, &doc)?;
        }
        // Membership finalization changes the registry in the background. Only
        // observe this exact denial from here through the end of the request.
        self.end_request();
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let doc: Registry = serde_json::from_slice(&self.document(false)?.1)?;
            if doc.current.revocations.contains(revocation) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(RuntimeError::Transport)
    }
    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
        self.change_registry(|registry| {
            if !registry.revocations.iter().any(|r| r.id == ack.revocation)
                || !registry.members.iter().any(|m| {
                    m.device_id == ack.server
                        && m.wg_verified
                        && matches!(m.kind, MemberKind::Server(_))
                })
            {
                return Err(RuntimeError::Invalid("ACK target or server"));
            }
            registry
                .acks
                .retain(|old| old.revocation != ack.revocation || old.server != ack.server);
            registry.acks.push(ack.clone());
            Ok(())
        })
    }
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        self.confirm()?;
        let bytes = self.document(true)?.1;
        if bytes.is_empty() {
            return Ok(SharedSnapshot::empty(self.config.network.id.clone()));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn append_record(&mut self, record: &SharedRecord) -> RuntimeResult<()> {
        self.confirm()?;
        let (version, bytes) = self.document(true)?;
        let network = self.config.network.id.clone();
        let mut shared = if bytes.is_empty() {
            SharedSnapshot::empty(network.clone())
        } else {
            serde_json::from_slice(&bytes)?
        };
        shared.merge(&SharedSnapshot {
            network,
            records: vec![record.clone()],
        })?;
        self.update(true, version, &shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn founder_reopens_committed_directory_and_refuses_erased_storage() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _entered = rt.enter();
        let f = super::super::tests::Fixture::new();
        let dir = crate::isolation::fixture::private_tempdir();
        let files = Directory::open(dir.path(), true).unwrap();
        let open = || {
            Catalog::start(
                &f.config,
                &f.config.bootstrap,
                &f.keys,
                dir.path(),
                &files,
                &AtomicBool::new(false),
                |_| std::net::TcpListener::bind("127.0.0.1:0"),
            )
        };
        let candidate = tako_core::StorageClaim {
            name: "orders".into(),
            engine: tako_core::DatabaseEngine::Turso,
            generation: "ab".repeat(16),
            created_at_unix_secs: 1,
            founders: [f.config.server_id.to_string()].into(),
            workflows_owner: None,
            channel_owner: None,
        };
        let mut catalog = open().unwrap();
        catalog.begin_request().unwrap();
        catalog
            .storage(tako_core::StorageOperation::Claim {
                candidate: candidate.clone(),
            })
            .unwrap();
        catalog.end_request();
        let shared = catalog.shared_snapshot().unwrap();
        assert_eq!(shared.records.len(), 2);
        let key = shared
            .records
            .iter()
            .find(|r| r.address.kind == tako_network::shared::RecordKind::DatabaseKey)
            .unwrap();
        assert!(
            key.open(&shared, &f.admin, &f.config.network.initial_admin)
                .unwrap()
                .is_some()
        );
        drop(catalog);
        let mut catalog = open().unwrap();
        assert_eq!(catalog.snapshot().unwrap(), f.config.bootstrap);
        assert_eq!(catalog.shared_snapshot().unwrap(), shared);
        catalog
            .storage(tako_core::StorageOperation::Retirement {
                generation: candidate.generation.clone(),
                targets: Some(candidate.founders.clone()),
            })
            .unwrap();
        let retire = || tako_core::StorageOperation::RetireName {
            name: candidate.name.clone(),
            generation: candidate.generation.clone(),
        };
        catalog.storage(retire()).unwrap();
        catalog.storage(retire()).unwrap();
        let retired = catalog.shared_snapshot().unwrap();
        assert_eq!(retired.records.len(), 2);
        assert!(retired.records.iter().any(|r| r.address.kind
            == tako_network::shared::RecordKind::Database
            && r.is_deletion()));
        assert!(retired.records.iter().any(|r| r == key));
        let mut replacement = candidate.clone();
        replacement.generation = "cd".repeat(16);
        catalog
            .storage(tako_core::StorageOperation::Claim {
                candidate: replacement,
            })
            .unwrap();
        catalog.storage(retire()).unwrap();
        assert_eq!(
            catalog
                .shared_snapshot()
                .unwrap()
                .records
                .iter()
                .filter(
                    |r| r.address.kind == tako_network::shared::RecordKind::Database
                        && !r.is_deletion()
                )
                .count(),
            1
        );
        drop(catalog);
        std::fs::remove_file(dir.path().join("catalog/raft.redb")).unwrap();
        assert!(open().is_err());
    }
}
