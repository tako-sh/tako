//! Agent-owned channel writer and authenticated direct WireGuard peer transport.
//! The membership input must contain only snapshots already enforced by roothelper.
mod broker;
mod ipc;
mod peer;
mod presence;
mod wire;

pub use ipc::{Client, Notification, Subscription};

use broker::Broker;
use std::{
    collections::BTreeMap,
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tako_network::{DeviceId, LocalDeviceKeys, NodeConfig, RegistrySnapshot};
use tokio::{
    net::{TcpListener, TcpSocket},
    sync::{mpsc, watch},
    task::JoinSet,
};

pub const PEER_PORT: u16 = 9850;
const MAX_CONNECTIONS: usize = 64;
const MAX_APPS: usize = 128;
const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_LOCAL_CONNECTIONS: usize = MAX_SUBSCRIPTIONS + 64;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn error(message: &'static str) -> io::Error {
    io::Error::other(message)
}

pub async fn run(
    data_dir: PathBuf,
    config: NodeConfig,
    mut membership: watch::Receiver<Option<RegistrySnapshot>>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    config
        .validate()
        .map_err(|_| error("invalid channel node configuration"))?;
    let directory = config.network_dir(&data_dir);
    // The protected directory check also rejects links in all ancestors.
    tako_network::node_config::protected::Directory::open(&directory, true)
        .map_err(|_| error("unsafe channel directory"))?;
    if config.files.names().contains(&"channels.sock")
        || config.files.names().contains(&"channels.lock")
    {
        return Err(error("reserved channel filename"));
    }
    let keys = Arc::new(
        LocalDeviceKeys::load(&directory.join(&config.files.keys))
            .map_err(|_| error("cannot load channel identity"))?,
    );
    if keys
        .device_id(&config.network.id, tako_network::DeviceKind::Server)
        .map_err(|_| error("invalid channel identity"))?
        != config.server_id
    {
        return Err(error("channel identity mismatch"));
    }
    let address = config
        .network
        .id
        .address(&keys.wg_public_key().map_err(|_| error("invalid WG key"))?);
    let (local_listener, socket_guard) = ipc::bind(&directory).await?;
    let (commands, receiver) = mpsc::channel(128);
    let inbound = wire::Budget::new(8 * wire::MAX_FRAME);
    let outbound = wire::Budget::new(16 * wire::MAX_FRAME);
    let replies = wire::Budget::new(8 * wire::MAX_FRAME);
    let notifications = wire::Budget::new(8 * wire::MAX_FRAME);
    let broker = Broker::new(
        PathBuf::from("/opt/tako/apps"),
        notifications,
        config.server_id.clone(),
    );
    let (control, control_rx) = watch::channel(broker::Control::default());
    let (broker_shutdown, broker_shutdown_rx) = watch::channel(false);
    let mut broker_task = tokio::spawn(async move {
        // Retain the exclusive writer lock until any in-flight database work finishes,
        // even if the transport supervisor is cancelled by its caller.
        let _socket_guard = socket_guard;
        broker::run(broker, receiver, control_rx, broker_shutdown_rx).await
    });
    let mut local_tasks = JoinSet::new();
    let mut peers = JoinSet::new();
    let mut roster = Arc::new(BTreeMap::new());
    let config = Arc::new(config);
    let mut admitted = false;
    let mut attempts: BTreeMap<DeviceId, (u32, tokio::time::Instant)> = BTreeMap::new();
    let mut pending = std::collections::BTreeSet::new();
    let mut listener = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut generation = 0u64;
    let mut refresh = true;
    let mut worker_finished = false;
    let result = async {
    loop {
        if *shutdown.borrow() { break; }
        if refresh {
            refresh = false;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let next_admitted = membership.borrow().is_some();
            let next = peer::roster(&config, membership.borrow().as_ref(), now)?;
            if next != *roster || next_admitted != admitted {
                generation = generation.wrapping_add(1);
                peers.abort_all();
                while peers.join_next().await.is_some() {}
                pending.clear();
                attempts.clear();
                control.send_replace(broker::Control { generation, losses: BTreeMap::new() });
                roster = Arc::new(next);
                admitted = next_admitted;
                listener = None;
            }
            if !roster.is_empty() && listener.is_none() {
                // No wildcard or public fallback. A temporarily absent WG address is retried.
                if let Ok(bound) = TcpListener::bind((address, PEER_PORT)).await { listener = Some(bound); }
            }
            let now = tokio::time::Instant::now();
            for (id, member) in roster.iter() {
                if id <= &config.server_id || pending.contains(id) || peers.len() >= MAX_CONNECTIONS { continue; }
                if attempts.get(id).is_some_and(|(_, next)| *next > now) { continue; }
                pending.insert(id.clone());
                let id = id.clone();
                let remote = config.network.id.address(&member.request.wg_public_key);
                let config = config.clone();
                let keys = keys.clone();
                let roster = roster.clone();
                let commands = commands.clone();
                let inbound = inbound.clone();
                let outbound = outbound.clone();
                peers.spawn(async move {
                    let connected = async {
                        let socket = TcpSocket::new_v6()?;
                        socket.bind((address, 0).into())?;
                        tokio::time::timeout(IO_TIMEOUT, socket.connect((remote, PEER_PORT).into())).await?
                    }.await;
                    let result = match connected {
                        Ok(stream) => peer::serve(stream, &config, &keys, &roster, Some(&id), generation, commands, inbound, outbound).await,
                        Err(error) => peer::SessionEnd::rejected(error),
                    };
                    (id, result)
                });
            }
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break; } }
            changed = membership.changed() => {
                if changed.is_err() { break; }
                refresh = true;
            }
            _ = tick.tick() => { refresh = true; }
            Some(result) = peers.join_next(), if !peers.is_empty() => {
                let (id, outcome) = result.map_err(|_| error("channel peer worker failed"))?;
                pending.remove(&id);
                if outcome.authenticated {
                    control.send_modify(|state| { *state.losses.entry(id.clone()).or_default() += 1; });
                }
                let failures = if outcome.authenticated { 1 } else { attempts.get(&id).map_or(0, |(n, _)| *n).saturating_add(1).min(5) };
                let _ = outcome.result;
                attempts.insert(id, (failures, tokio::time::Instant::now() + Duration::from_secs(1 << failures)));
            }
            Some(_) = local_tasks.join_next(), if !local_tasks.is_empty() => {}
            accepted = local_listener.accept(), if local_tasks.len() < MAX_LOCAL_CONNECTIONS => {
                let (stream, _) = accepted?;
                if ipc::authorized(&stream)? {
                    let commands = commands.clone();
                    let inbound = inbound.clone();
                    let replies = replies.clone();
                    let signer = Some((keys.clone(), config.network.id.clone()));
                    local_tasks.spawn(async move { let _ = ipc::serve_with_signer(stream, commands, inbound, replies, signer).await; });
                }
            }
            accepted = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() && peers.len() < MAX_CONNECTIONS => {
                let (stream, remote) = accepted?;
                let candidate = roster.iter().find(|(id, member)| *id < &config.server_id
                    && std::net::IpAddr::V6(config.network.id.address(&member.request.wg_public_key)) == remote.ip())
                    .map(|(id, _)| id.clone());
                if let Some(id) = candidate.filter(|id| !pending.contains(id)
                    && attempts.get(id).is_none_or(|(_, retry)| *retry <= tokio::time::Instant::now())) {
                    pending.insert(id.clone());
                    let config = config.clone();
                    let keys = keys.clone();
                    let roster = roster.clone();
                    let commands = commands.clone();
                    let inbound = inbound.clone();
                    let outbound = outbound.clone();
                    peers.spawn(async move {
                        let result = peer::serve(stream, &config, &keys, &roster, None, generation, commands, inbound, outbound).await;
                        (id, result)
                    });
                }
            }
            result = &mut broker_task => { worker_finished = true; return result.map_err(|_| error("channel broker worker failed"))?; }
        }
    }
    Ok(())
    }.await;
    peers.abort_all();
    local_tasks.abort_all();
    broker_shutdown.send_replace(true);
    let worker_result = if worker_finished {
        Ok(())
    } else {
        broker_task
            .await
            .map_err(|_| error("channel broker worker failed"))?
    };
    result.and(worker_result)
}

#[cfg(test)]
mod tests;
