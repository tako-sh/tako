//! Agent-owned, credential-free discovery projection and durable lease publication.
//! Signing and Corrosion calls remain on the existing serialized controller.
use super::{AgentError, Result};
use crate::native_services::{ComposeGateway, LocalServices, ServiceDirectory, ServiceSnapshot};
use parking_lot::{Mutex, RwLock};
use std::{collections::BTreeSet, path::Path, sync::Arc};
use tako_network::{
    AppEnv, LocalDeviceKeys, MemberKind, NodeConfig, RegistrySnapshot,
    corrosion::{CorrosionStore, MAX_REGISTRY_BYTES},
    node_config::protected::Directory,
    services::{MAX_LEASE_BYTES, RENEW_SECONDS, ServiceLease},
};

pub(super) const LOCAL_FILE: &str = "local-services.json";
pub(super) const SNAPSHOT_FILE: &str = "services.json";
const JOURNAL_FILE: &str = "service-publication.json";
pub(super) const MAX_SNAPSHOT_BYTES: usize = MAX_REGISTRY_BYTES + 256 * MAX_LEASE_BYTES + 1024;

pub(super) struct Shared {
    pub directory: Arc<RwLock<ServiceDirectory>>,
    files: Directory,
    // Serialize disk publication/invalidation with the in-process projection.
    publication: Mutex<()>,
}
impl Shared {
    pub fn open(config: &NodeConfig, data_dir: &Path) -> Result<Arc<Self>> {
        let shared = Arc::new(Self {
            directory: Arc::new(RwLock::new(ServiceDirectory::new(
                config.network.id.clone(),
                config.server_id.clone(),
                Some(ComposeGateway::for_network(&config.network.id)),
            ))),
            files: Directory::open(&config.network_dir(data_dir), true)?,
            publication: Mutex::new(()),
        });
        shared.clear()?;
        Ok(shared)
    }
    pub fn clear(&self) -> Result<()> {
        let _publication = self.publication.lock();
        self.directory.write().clear();
        // Proxy reads Option<ServiceSnapshot>; null is an explicit withdrawal.
        self.files.write(SNAPSHOT_FILE, b"null\n")?;
        Ok(())
    }
    /// Caller holds the enforced-membership read gate through this operation.
    pub fn publish(&self, snapshot: &ServiceSnapshot, now: u64) -> Result<()> {
        let _publication = self.publication.lock();
        let bytes = serde_json::to_vec(snapshot)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            self.directory.write().clear();
            self.files.write(SNAPSHOT_FILE, b"null\n")?;
            return Err(AgentError::Invalid("service snapshot exceeds bound"));
        }
        let mut view = self.directory.write();
        if view.replace_snapshot(snapshot, now).is_err() {
            self.files.write(SNAPSHOT_FILE, b"null\n")?;
            return Err(AgentError::Invalid("invalid service snapshot"));
        }
        if let Err(error) = self.files.write(SNAPSHOT_FILE, &bytes) {
            view.clear();
            return Err(error.into());
        }
        Ok(())
    }
}

pub(super) struct Publisher {
    files: Directory,
    previous: Option<ServiceLease>,
    acknowledged: bool,
}
impl Publisher {
    pub fn open(config: &NodeConfig, data_dir: &Path) -> Result<Self> {
        let files = Directory::open(&config.network_dir(data_dir), true)?;
        let previous = if files.exists(JOURNAL_FILE)? {
            let lease = ServiceLease::from_slice(&files.read(JOURNAL_FILE, MAX_LEASE_BYTES)?)?;
            let member = config
                .bootstrap
                .members
                .iter()
                .find(|m| m.device_id == config.server_id)
                .ok_or(AgentError::Invalid(
                    "service publisher missing from bootstrap",
                ))?;
            // Verify historical public envelope without treating it as live authority.
            lease.verify_member(&config.network.id, member, lease.issued_at)?;
            if lease.server_id != config.server_id {
                return Err(AgentError::Invalid("service journal publisher mismatch"));
            }
            Some(lease)
        } else {
            None
        };
        Ok(Self {
            files,
            previous,
            acknowledged: false,
        })
    }
    fn local_apps(&self, now: u64) -> BTreeSet<AppEnv> {
        let value = self
            .files
            .read(LOCAL_FILE, MAX_LEASE_BYTES)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LocalServices>(&bytes).ok());
        match value {
            Some(value) if value.validate(now).is_ok() => value.apps,
            _ => BTreeSet::new(),
        }
    }
    /// Persist the exact signed envelope before SQL. A transport failure or
    /// process restart can retry it without issuing a same-second fork.
    fn publish_with(
        &mut self,
        config: &NodeConfig,
        keys: &LocalDeviceKeys,
        apps: &BTreeSet<AppEnv>,
        now: u64,
        mut publish: impl FnMut(&ServiceLease) -> Result<()>,
    ) -> Result<()> {
        let changed = self
            .previous
            .as_ref()
            .is_none_or(|old| old.apps.iter().cloned().collect::<BTreeSet<_>>() != *apps);
        if self
            .previous
            .as_ref()
            .is_some_and(|old| old.issued_at > now)
        {
            return Err(AgentError::Invalid(
                "service publication clock moved backwards",
            ));
        }
        // A changed export within the same second waits; it never republishes an
        // old positive lease on behalf of missing/stale local deployment state.
        if changed
            && self
                .previous
                .as_ref()
                .is_some_and(|old| old.issued_at == now)
        {
            return Ok(());
        }
        if changed
            || self
                .previous
                .as_ref()
                .is_none_or(|old| now.saturating_sub(old.issued_at) >= RENEW_SECONDS)
        {
            let lease = ServiceLease::sign(keys, config.network.id.clone(), apps.clone(), now)?;
            let bytes = serde_json::to_vec(&lease)?;
            if bytes.len() > MAX_LEASE_BYTES {
                return Err(AgentError::Invalid("service publication exceeds bound"));
            }
            self.files.write(JOURNAL_FILE, &bytes)?;
            self.previous = Some(lease);
            self.acknowledged = false;
        }
        if !self.acknowledged {
            if let Some(lease) = &self.previous {
                publish(lease)?;
                self.acknowledged = true;
            }
        }
        Ok(())
    }
    pub fn refresh(
        &mut self,
        config: &NodeConfig,
        keys: &LocalDeviceKeys,
        store: &CorrosionStore,
        registry: &RegistrySnapshot,
        now: u64,
    ) -> Result<Vec<ServiceLease>> {
        let effective = registry.effective_members(now)?;
        if !effective
            .get(&config.server_id)
            .is_some_and(|m| matches!(m.kind, MemberKind::Server(_)) && m.permissions.server_admin)
        {
            return Err(AgentError::Invalid(
                "local service publisher is not effective",
            ));
        }
        let apps = self.local_apps(now);
        self.publish_with(config, keys, &apps, now, |lease| {
            store.publish_services(lease).map_err(Into::into)
        })?;
        let mut leases = store.service_leases(registry, now)?;
        // Until a changed empty/new local lease reaches SQL, do not route locally
        // through our old deployment export. Remote leases retain their own TTLs.
        leases.retain(|lease| {
            lease.server_id != config.server_id
                || lease.apps.iter().cloned().collect::<BTreeSet<_>>() == apps
        });
        Ok(leases)
    }
}

/// One DNS owner for the whole agent lifetime. Both listeners start only after
/// host enforcement, and both are drained before the controller releases its DB.
pub(super) async fn run(
    shared: Arc<Shared>,
    config: NodeConfig,
    mut membership: tokio::sync::watch::Receiver<Option<RegistrySnapshot>>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    control: super::lifecycle::Control,
) -> std::io::Result<()> {
    use crate::native_services::{DnsView, host_nameservers, serve_dns};
    use std::net::{IpAddr, SocketAddr};
    if !wait_enforced(&mut membership, shutdown.clone()).await {
        return Ok(());
    }
    let member = config
        .bootstrap
        .members
        .iter()
        .find(|member| member.device_id == config.server_id)
        .ok_or_else(|| std::io::Error::other("DNS server absent from prepared identity"))?;
    let address = config.network.id.address(&member.request.wg_public_key);
    let gateway = ComposeGateway::for_network(&config.network.id).address();
    let native = serve_dns(
        shared.directory.clone(),
        SocketAddr::new(address.into(), 53),
        DnsView::Native,
        shutdown.clone(),
    );
    let compose = retry_compose(shutdown.clone(), || {
        let directory = shared.directory.clone();
        let shutdown = shutdown.clone();
        // Do not feed a gateway/native peer back into the host forwarding path.
        let mut excluded = vec![IpAddr::V4(gateway), IpAddr::V6(address)];
        if let Some(snapshot) = membership.borrow().as_ref() {
            excluded.extend(
                snapshot
                    .members
                    .iter()
                    .map(|m| IpAddr::V6(config.network.id.address(&m.request.wg_public_key))),
            );
        }
        async move {
            let upstreams = host_nameservers()
                .ok()
                .map(|mut upstreams| {
                    upstreams.retain(|ip| !excluded.contains(ip));
                    upstreams
                })
                .filter(|upstreams| !upstreams.is_empty())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::AddrNotAvailable,
                        "Compose DNS waiting for usable host nameservers",
                    )
                })?;
            serve_dns(
                directory,
                SocketAddr::new(gateway.into(), 53),
                DnsView::Compose { gateway, upstreams },
                shutdown,
            )
            .await
        }
    });
    let (native, compose) = tokio::join!(
        supervise_dns(native, &control),
        supervise_dns(compose, &control)
    );
    native.and(compose)
}

async fn supervise_dns(
    future: impl std::future::Future<Output = std::io::Result<()>>,
    control: &super::lifecycle::Control,
) -> std::io::Result<()> {
    let result = future.await;
    let unexpected = !control.stop.load(std::sync::atomic::Ordering::Acquire);
    if result.is_err() || unexpected {
        control.shutdown(true);
    }
    if unexpected && result.is_ok() {
        return Err(std::io::Error::other("DNS listener exited unexpectedly"));
    }
    result
}

async fn stopping(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_enforced(
    membership: &mut tokio::sync::watch::Receiver<Option<RegistrySnapshot>>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        biased;
        _ = stopping(shutdown) => false,
        result = membership.wait_for(Option::is_some) => result.is_ok(),
    }
}

async fn retry_compose<F, Fut>(
    shutdown: tokio::sync::watch::Receiver<bool>,
    mut attempt: F,
) -> std::io::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    let mut reported = false;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        // serve_dns owns draining its jobs; don't cancel a running listener.
        match attempt().await {
            Err(error) if error.kind() == std::io::ErrorKind::AddrNotAvailable => {
                if !reported {
                    tracing::warn!(%error, "Compose DNS dependency unavailable; retrying without stopping native DNS");
                    reported = true;
                }
            }
            result => return result,
        }
        tokio::select! {
            biased;
            _ = stopping(shutdown.clone()) => return Ok(()),
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => (),
        }
    }
}
