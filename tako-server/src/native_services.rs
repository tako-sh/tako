mod dns;
mod export;

pub use dns::{DnsView, host_nameservers, serve_dns};
pub use export::LocalServices;

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::{Duration, Instant},
};
use tako_network::services::{MAX_LEASE_SECONDS, ServiceHost, ServiceLease, classify_host};
use tako_network::{AppEnv, DeviceId, MemberKind, NetworkId, RegistrySnapshot};

const MAX_SERVERS: usize = 256;

#[derive(Clone, Debug)]
pub struct ComposeGateway {
    address: Ipv4Addr,
    subnet: ipnet::Ipv4Net,
}

impl ComposeGateway {
    pub fn for_network(network: &NetworkId) -> Self {
        let settings =
            tako_compose::NativeNetwork::new(&network.to_string()).expect("canonical NetworkId");
        Self::new(
            settings.gateway(),
            settings.subnet().parse().expect("derived subnet"),
        )
        .expect("derived gateway")
    }

    pub fn address(&self) -> Ipv4Addr {
        self.address
    }

    fn new(address: Ipv4Addr, subnet: ipnet::Ipv4Net) -> Result<Self, &'static str> {
        if !address.is_private()
            || !subnet.contains(&address)
            || address == subnet.network()
            || address == subnet.broadcast()
            || subnet.prefix_len() < 16
            || subnet.prefix_len() > 29
        {
            return Err("invalid managed Compose bridge");
        }
        Ok(Self { address, subnet })
    }

    fn admits(&self, destination: IpAddr, peer: IpAddr) -> bool {
        destination == IpAddr::V4(self.address)
            && matches!(peer, IpAddr::V4(peer)
            if self.subnet.contains(&peer) && peer != self.subnet.network()
                && peer != self.subnet.broadcast() && peer != self.address)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub server_id: DeviceId,
    pub address: Ipv6Addr,
    pub expires_at: u64,
    deadline: Instant,
}

impl Placement {
    pub fn ttl(&self, now: u64) -> u32 {
        self.expires_at
            .saturating_sub(now)
            .min(
                self.deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs(),
            )
            .min(5) as u32
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivateRoute {
    Public,
    Forbidden,
    Unavailable,
    Local(AppEnv),
    Remote { app: AppEnv, address: Ipv6Addr },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSnapshot {
    pub observed_at: u64,
    pub registry: RegistrySnapshot,
    pub leases: Vec<ServiceLease>,
}

struct View {
    address: Ipv6Addr,
    peers: BTreeSet<IpAddr>,
    placements: BTreeMap<AppEnv, Vec<Placement>>,
    observed_at: u64,
    valid_until: u64,
    deadline: Instant,
}

pub struct ServiceDirectory {
    network: NetworkId,
    local_server: DeviceId,
    gateway: Option<ComposeGateway>,
    view: Option<View>,
    latest: BTreeMap<DeviceId, (ServiceLease, Instant)>,
    envelope: Option<(u64, Instant)>,
    clock_floor: u64,
}

impl ServiceDirectory {
    pub fn new(
        network: NetworkId,
        local_server: DeviceId,
        gateway: Option<ComposeGateway>,
    ) -> Self {
        Self {
            network,
            local_server,
            gateway,
            view: None,
            latest: BTreeMap::new(),
            envelope: None,
            clock_floor: 0,
        }
    }

    pub fn network(&self) -> &NetworkId {
        &self.network
    }

    pub fn clear(&mut self) {
        self.view = None;
    }

    pub fn replace_snapshot(&mut self, snapshot: &ServiceSnapshot, now: u64) -> Result<(), String> {
        self.clear();
        let expires_at = snapshot.observed_at.saturating_add(15);
        if snapshot.observed_at > now || now >= expires_at {
            return Err("stale service snapshot".into());
        }
        let proposed = Instant::now() + Duration::from_secs(expires_at - now);
        let deadline = match self.envelope {
            Some((observed, _)) if observed > snapshot.observed_at => {
                return Err("service snapshot rolled back".into());
            }
            Some((observed, deadline)) if observed == snapshot.observed_at => {
                deadline.min(proposed)
            }
            _ => proposed,
        };
        if deadline <= Instant::now() {
            return Err("service snapshot expired".into());
        }
        self.replace(&snapshot.registry, &snapshot.leases, now)?;
        let view = self.view.as_mut().expect("replace installed view");
        view.valid_until = view.valid_until.min(expires_at);
        view.deadline = view.deadline.min(deadline);
        self.envelope = Some((snapshot.observed_at, deadline));
        Ok(())
    }

    /// Membership must have passed the agent's monotonic guard and host enforcement.
    pub fn replace(
        &mut self,
        registry: &RegistrySnapshot,
        leases: &[ServiceLease],
        now: u64,
    ) -> Result<(), String> {
        self.clear();
        if now < self.clock_floor {
            return Err("service clock moved backwards".into());
        }
        self.clock_floor = now;
        self.latest.retain(|_, (lease, _)| lease.expires_at > now);
        if registry.network.id != self.network || leases.len() > MAX_SERVERS {
            return Err("invalid service snapshot network or size".into());
        }
        let effective = registry.effective_members(now).map_err(|e| e.to_string())?;
        let local = effective
            .get(&self.local_server)
            .filter(|m| matches!(m.kind, MemberKind::Server(_)) && m.permissions.server_admin)
            .ok_or("local server is not admitted")?;
        let address = self.network.address(&local.request.wg_public_key);
        let mut peers = BTreeSet::new();
        let mut valid_until = now.saturating_add(MAX_LEASE_SECONDS);
        for member in effective.values() {
            peers.insert(IpAddr::V6(
                self.network.address(&member.request.wg_public_key),
            ));
            if let Some(expiry) = member.expires_at {
                valid_until = valid_until.min(expiry);
            }
        }
        let mut placements: BTreeMap<AppEnv, Vec<Placement>> = BTreeMap::new();
        let mut next = BTreeMap::new();
        for lease in leases {
            let Some(member) = effective.get(&lease.server_id) else {
                continue;
            };
            let Ok(address) = lease.verify_member(&self.network, member, now) else {
                continue;
            };
            let mut deadline = Instant::now() + Duration::from_secs(lease.expires_at - now);
            if let Some((previous, previous_deadline)) = self.latest.get(&lease.server_id) {
                if lease.issued_at < previous.issued_at {
                    continue;
                }
                if lease.issued_at == previous.issued_at {
                    deadline = deadline.min(*previous_deadline);
                }
                if lease.issued_at == previous.issued_at && lease != previous {
                    return Err("conflicting service leases at the same timestamp".into());
                }
            }
            if next
                .insert(lease.server_id.clone(), (lease.clone(), deadline))
                .is_some()
            {
                return Err("duplicate service publisher".into());
            }
            for app in &lease.apps {
                placements.entry(app.clone()).or_default().push(Placement {
                    server_id: lease.server_id.clone(),
                    address,
                    expires_at: lease.expires_at.min(valid_until),
                    deadline,
                });
            }
        }
        if self
            .latest
            .keys()
            .chain(next.keys())
            .collect::<BTreeSet<_>>()
            .len()
            > MAX_SERVERS
        {
            return Err("too many observed service publishers".into());
        }
        self.latest.extend(next);
        for placements in placements.values_mut() {
            placements.sort_by_key(|p| p.address);
        }
        self.view = Some(View {
            address,
            peers,
            placements,
            observed_at: now,
            valid_until,
            deadline: Instant::now() + Duration::from_secs(valid_until.saturating_sub(now)),
        });
        Ok(())
    }

    fn current(&self, now: u64) -> Option<&View> {
        self.view
            .as_ref()
            .filter(|v| now >= v.observed_at && now < v.valid_until && Instant::now() < v.deadline)
    }

    pub fn placements(&self, app: &AppEnv, now: u64) -> Vec<Placement> {
        let Some(view) = self.current(now) else {
            return Vec::new();
        };
        view.placements
            .get(app)
            .into_iter()
            .flatten()
            .filter(|p| p.expires_at > now && Instant::now() < p.deadline)
            .cloned()
            .map(|mut p| {
                p.expires_at = p.expires_at.min(view.valid_until);
                p.deadline = p.deadline.min(view.deadline);
                p
            })
            .collect()
    }

    pub fn admits(&self, destination: IpAddr, peer: IpAddr, now: u64) -> bool {
        let Some(view) = self.current(now) else {
            return false;
        };
        (destination == IpAddr::V6(view.address) && view.peers.contains(&peer))
            || self
                .gateway
                .as_ref()
                .is_some_and(|g| g.admits(destination, peer))
    }

    fn admits_dns(&self, destination: IpAddr, peer: IpAddr, now: u64) -> bool {
        self.gateway
            .as_ref()
            .is_some_and(|g| g.admits(destination, peer))
            || self.admits(destination, peer, now)
    }

    pub fn route(
        &self,
        authority: &str,
        destination: IpAddr,
        peer: IpAddr,
        now: u64,
    ) -> PrivateRoute {
        let destination = destination.to_canonical();
        let peer = peer.to_canonical();
        let app = match classify_host(authority, &self.network) {
            ServiceHost::Public => return PrivateRoute::Public,
            ServiceHost::Reserved => return PrivateRoute::Forbidden,
            ServiceHost::App(app) => app,
        };
        if !self.admits(destination, peer, now) {
            return PrivateRoute::Forbidden;
        }
        let placements = self.placements(&app, now);
        if placements.iter().any(|p| p.server_id == self.local_server) {
            return PrivateRoute::Local(app);
        }
        // Only the container entrypoint may relay. A peer request cannot loop.
        if self
            .gateway
            .as_ref()
            .is_some_and(|g| g.admits(destination, peer))
        {
            if let Some(placement) = placements.first() {
                return PrivateRoute::Remote {
                    app,
                    address: placement.address,
                };
            }
        }
        PrivateRoute::Unavailable
    }
}

#[cfg(test)]
mod tests;
