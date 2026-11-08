use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use tako_network::node_config::protected::Directory;
use tako_network::{MemberKind, NodeConfig};

use crate::ServerState;
use crate::native_services::{
    ComposeGateway, LocalServices, PrivateRoute, ServiceDirectory, ServiceSnapshot,
};

pub(crate) struct NativeProxy {
    directory: RwLock<ServiceDirectory>,
    path: PathBuf,
    /// Only the primary publishes the local inventory. A standby shares the
    /// data directory, but its application set stops at its own startup, so
    /// owning the export would freeze what the network sees from this host.
    export_owner: Option<String>,
    export_replaced: AtomicBool,
    pub(crate) address: Ipv6Addr,
    pub(crate) gateway: Ipv4Addr,
}

impl NativeProxy {
    pub(crate) fn start(
        runtime: &tokio::runtime::Runtime,
        state: Arc<ServerState>,
        config: &NodeConfig,
        draining: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let member = config
            .bootstrap
            .members
            .iter()
            .find(|member| {
                member.device_id == config.server_id && matches!(member.kind, MemberKind::Server(_))
            })
            .ok_or("native service identity is absent")?;
        let gateway = ComposeGateway::for_network(&config.network.id);
        let service = Arc::new(Self {
            directory: RwLock::new(ServiceDirectory::new(
                config.network.id.clone(),
                config.server_id.clone(),
                Some(gateway.clone()),
            )),
            path: config.network_dir(&state.runtime_config().data_dir),
            export_owner: (!state.runtime_config().standby).then(|| nanoid::nanoid!(32)),
            export_replaced: AtomicBool::new(false),
            address: config.network.id.address(&member.request.wg_public_key),
            gateway: gateway.address(),
        });
        let directory = Directory::open(&service.path, true)?;
        {
            let router = Arc::downgrade(&service);
            state
                .workflows
                .set_network_router(Arc::new(move |target, invocation| {
                    let Some(router) = router.upgrade() else {
                        return tako_core::Response::error("network router unavailable");
                    };
                    let scope = match crate::native_channel_client::app_scope(target) {
                        Ok(scope) => scope,
                        Err(_) => return tako_core::Response::error("invalid destination app"),
                    };
                    let (placement, network) = {
                        let directory = router.directory.read();
                        (
                            directory.placements(&scope, now()).into_iter().next(),
                            directory.network().to_string(),
                        )
                    };
                    let Some(placement) = placement else {
                        return tako_core::Response::error(
                            "destination app unavailable on this network",
                        );
                    };
                    let (app, environment) =
                        target.split_once('/').unwrap_or((target, "production"));
                    tokio::runtime::Handle::current().block_on(
                        tako_workflows::network::invoke_http(
                            &format!("http://[{}]/_tako/network", placement.address),
                            &format!("{app}.{environment}.{network}.tako.internal"),
                            None,
                            &invocation,
                        ),
                    )
                }));
        }
        if let Some(owner) = &service.export_owner {
            claim_export(&directory, owner)?;
        }
        let background = service.clone();
        runtime.spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(2));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut failed = false;
            loop {
                timer.tick().await;
                let active = background.clone();
                let state = state.clone();
                let draining = draining.clone();
                let result = tokio::task::spawn_blocking(move || {
                    active.refresh(&state, draining.load(Ordering::Acquire))
                })
                .await;
                let error = match result {
                    Ok(Ok(())) => {
                        failed = false;
                        continue;
                    }
                    Ok(Err(error)) => error,
                    Err(error) => error.to_string(),
                };
                // Expiry also protects requests if this task cannot run again.
                background.clear();
                if !failed {
                    tracing::warn!(%error, "Native service projection unavailable");
                    failed = true;
                }
            }
        });
        Ok(service)
    }

    fn refresh(&self, state: &ServerState, draining: bool) -> Result<(), String> {
        let now = now();
        let directory = Directory::open(&self.path, true).map_err(|error| error.to_string())?;
        if let Some(owner) = &self.export_owner {
            let export = local_services(state.app_manager().as_ref(), draining, now);
            export.validate(now).map_err(str::to_string)?;
            let bytes = serde_json::to_vec(&export).map_err(|error| error.to_string())?;
            let published =
                publish_export(&directory, owner, &bytes).map_err(|error| error.to_string())?;
            if !published && !self.export_replaced.swap(true, Ordering::AcqRel) {
                tracing::info!("Native service export now belongs to a replacement server");
            }
        }
        let projection = directory
            .read("services.json", 32 * 1024 * 1024 + 1024)
            .map_err(|error| error.to_string())?;
        match serde_json::from_slice::<Option<ServiceSnapshot>>(&projection)
            .map_err(|error| error.to_string())?
        {
            Some(snapshot) => self.directory.write().replace_snapshot(&snapshot, now),
            None => {
                self.clear();
                Ok(())
            }
        }
    }

    pub(crate) fn clear(&self) {
        self.directory.write().clear();
    }

    pub(crate) fn route(
        &self,
        host: &str,
        destination: Option<IpAddr>,
        peer: Option<IpAddr>,
    ) -> PrivateRoute {
        if !reserved_host(host) {
            return PrivateRoute::Public;
        }
        match (destination, peer) {
            (Some(destination), Some(peer)) => {
                self.directory.read().route(host, destination, peer, now())
            }
            _ => PrivateRoute::Forbidden,
        }
    }
}

fn claim_export(directory: &Directory, owner: &str) -> tako_network::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let _guard = loop {
        match directory.lock("local-services-export.lock") {
            Ok(guard) => break guard,
            Err(tako_network::Error::Invalid {
                field: "agent lock",
                ..
            }) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    };
    directory.write("local-services-owner.json", &serde_json::to_vec(owner)?)
}

fn publish_export(directory: &Directory, owner: &str, bytes: &[u8]) -> tako_network::Result<bool> {
    let _guard = directory.lock("local-services-export.lock")?;
    let current: String =
        serde_json::from_slice(&directory.read("local-services-owner.json", 128)?)?;
    if current != owner {
        return Ok(false);
    }
    directory.write("local-services.json", bytes)?;
    Ok(true)
}

fn local_services(
    manager: &crate::instances::AppManager,
    draining: bool,
    now: u64,
) -> LocalServices {
    let mut apps = std::collections::BTreeSet::new();
    if !draining {
        for name in manager.list_apps() {
            let Some(app) = manager.get_app(&name) else {
                continue;
            };
            let eligible = match app.state() {
                crate::socket::AppState::Running | crate::socket::AppState::Deploying => {
                    app.healthy_backend_for_request(0).is_some()
                }
                crate::socket::AppState::Idle => app.config.read().min_instances == 0,
                _ => false,
            };
            if eligible && let Ok(app) = crate::native_channel_client::app_scope(&name) {
                apps.insert(app);
            }
        }
    }
    LocalServices {
        observed_at: now,
        apps,
    }
}

pub(crate) fn reserved_host(authority: &str) -> bool {
    let host = authority
        .split(':')
        .next()
        .unwrap_or(authority)
        .trim()
        .trim_end_matches('.');
    host.eq_ignore_ascii_case("tako.internal")
        || host.to_ascii_lowercase().ends_with(".tako.internal")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(path: &std::path::Path, export_owner: Option<&str>) -> NativeProxy {
        let keys = tako_network::LocalDeviceKeys::generate().unwrap();
        let network = tako_network::NetworkId::random().unwrap();
        let server = keys
            .device_id(&network, tako_network::DeviceKind::Server)
            .unwrap();
        NativeProxy {
            directory: RwLock::new(ServiceDirectory::new(network, server, None)),
            path: path.to_path_buf(),
            export_owner: export_owner.map(str::to_string),
            export_replaced: AtomicBool::new(false),
            address: Ipv6Addr::LOCALHOST,
            gateway: Ipv4Addr::LOCALHOST,
        }
    }

    #[test]
    fn standby_proxy_consumes_the_projection_without_taking_the_export() {
        let temp = crate::isolation::fixture::private_tempdir();
        let data = crate::isolation::fixture::TestDataDir::new().unwrap();
        let state = crate::ServerState::new(
            data.path().into(),
            Arc::new(crate::tls::CertManager::new(
                crate::tls::CertManagerConfig {
                    cert_dir: data.path().join("certs"),
                    ..Default::default()
                },
            )),
            None,
            Arc::new(RwLock::new(std::collections::HashMap::new())),
        )
        .unwrap();
        let directory = Directory::open(temp.path(), true).unwrap();
        claim_export(&directory, "primary").unwrap();
        assert!(publish_export(&directory, "primary", b"primary apps").unwrap());
        directory.write("services.json", b"null").unwrap();
        // A standby that starts after the primary only reads the projection.
        proxy(temp.path(), None).refresh(&state, false).unwrap();
        assert_eq!(
            directory
                .read("local-services.json", 128)
                .unwrap()
                .as_slice(),
            b"primary apps"
        );
        let owner: String =
            serde_json::from_slice(&directory.read("local-services-owner.json", 128).unwrap())
                .unwrap();
        assert_eq!(owner, "primary");
        // The primary keeps publishing its current inventory meanwhile.
        proxy(temp.path(), Some("primary"))
            .refresh(&state, false)
            .unwrap();
        let export: LocalServices =
            serde_json::from_slice(&directory.read("local-services.json", 4096).unwrap()).unwrap();
        assert!(export.apps.is_empty());
    }

    #[test]
    fn old_proxy_cannot_overwrite_a_replacement_export() {
        let temp = crate::isolation::fixture::private_tempdir();
        let directory = Directory::open(temp.path(), true).unwrap();
        claim_export(&directory, "old").unwrap();
        assert!(publish_export(&directory, "old", b"old apps").unwrap());
        claim_export(&directory, "new").unwrap();
        assert!(publish_export(&directory, "new", b"current apps").unwrap());
        assert!(!publish_export(&directory, "old", b"empty old shutdown").unwrap());
        assert_eq!(
            directory
                .read("local-services.json", 128)
                .unwrap()
                .as_slice(),
            b"current apps"
        );
    }

    #[test]
    fn reserved_namespace_cannot_fall_through_to_public_wildcards() {
        for host in [
            "tako.internal",
            "app.prod.net.tako.internal",
            "APP.PROD.NET.TAKO.INTERNAL.:80",
            "node.tako.internal:bad",
        ] {
            assert!(reserved_host(host), "{host}");
        }
        for host in [
            "example.com",
            "tako.internal.attacker.test",
            "not-tako.internal",
        ] {
            assert!(!reserved_host(host));
        }
    }
}
