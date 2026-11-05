use super::{IO_TIMEOUT, broker::Command, error, wire};
use serde::{Deserialize, Serialize};
use ssh_key::rand_core::{OsRng, RngCore};
use std::{collections::BTreeMap, io, net::IpAddr};
use tako_channels::ChannelPublication;
use tako_network::{
    AppEnv, DeviceId, LocalDeviceKeys, Member, MemberKind, NetworkId, NodeConfig, RegistrySnapshot,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{mpsc, oneshot},
};

pub(super) const NAMESPACE: &str = "tako-channel-peer-v1";
pub(super) type Roster = BTreeMap<DeviceId, Member>;

pub(super) fn roster(
    config: &NodeConfig,
    snapshot: Option<&RegistrySnapshot>,
    now: u64,
) -> io::Result<Roster> {
    let Some(snapshot) = snapshot else {
        return Ok(BTreeMap::new());
    };
    if snapshot.network != config.network {
        return Err(error("channel membership network mismatch"));
    }
    let effective = snapshot
        .effective_members(now)
        .map_err(|_| error("invalid channel membership"))?;
    let is_server = |m: &&Member| m.wg_verified && matches!(m.kind, MemberKind::Server(_));
    if !effective.get(&config.server_id).is_some_and(is_server) {
        return Ok(BTreeMap::new());
    }
    if effective.values().filter(|m| is_server(m)).count() > super::MAX_CONNECTIONS {
        return Err(error("channel peer limit exceeded"));
    }
    Ok(effective
        .into_iter()
        .filter(|(id, member)| id != &config.server_id && is_server(member))
        .map(|(id, member)| (id, member.clone()))
        .collect())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Hello {
    pub network: NetworkId,
    pub server: DeviceId,
    pub challenge: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    signature: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Frame {
    Publication {
        app: AppEnv,
        publication: ChannelPublication,
    },
    Gap {
        app: AppEnv,
    },
    Presence {
        app: AppEnv,
        publication: super::presence::Publication,
    },
    Keepalive,
}

pub(super) fn transcript(dialer: &Hello, acceptor: &Hello, direction: &str) -> io::Result<Vec<u8>> {
    // Length-delimited canonical serde tuple, fixed order and role-separated signatures.
    serde_json::to_vec(&(
        NAMESPACE,
        &dialer.network,
        &dialer.server,
        &acceptor.server,
        dialer.challenge,
        acceptor.challenge,
        direction,
    ))
    .map_err(|_| error("invalid peer transcript"))
}
pub(super) fn verify(
    key: &tako_network::SshPublicKey,
    bytes: &[u8],
    signature: &str,
) -> io::Result<()> {
    let public =
        ssh_key::PublicKey::from_openssh(key.as_str()).map_err(|_| error("invalid peer key"))?;
    let sig = ssh_key::SshSig::from_pem(signature).map_err(|_| error("invalid peer proof"))?;
    public
        .verify(NAMESPACE, bytes, &sig)
        .map_err(|_| error("peer authentication failed"))
}

pub(super) async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    network: &NetworkId,
    local: &DeviceId,
    keys: &LocalDeviceKeys,
    roster: &Roster,
    remote_ip: IpAddr,
    expected: Option<&DeviceId>,
) -> io::Result<DeviceId> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let mut challenge = [0; 32];
        OsRng
            .try_fill_bytes(&mut challenge)
            .map_err(|_| error("peer entropy unavailable"))?;
        let hello = Hello {
            network: network.clone(),
            server: local.clone(),
            challenge,
        };
        wire::write(stream, &hello).await?;
        let budget = wire::Budget::new(8192);
        let remote: Hello = wire::read(stream, &budget).await?.value;
        if &remote.network != network
            || remote.server == *local
            || expected.is_some_and(|id| id != &remote.server)
            || (expected.is_some() != (*local < remote.server))
        {
            return Err(error("unexpected channel peer"));
        }
        let member = roster
            .get(&remote.server)
            .ok_or_else(|| error("unadmitted channel peer"))?;
        if !member.wg_verified
            || !matches!(member.kind, MemberKind::Server(_))
            || member.network_id != *network
            || IpAddr::V6(network.address(&member.request.wg_public_key)) != remote_ip
        {
            return Err(error("channel peer identity or address mismatch"));
        }
        let (dialer, acceptor, our_role, their_role) = if expected.is_some() {
            (&hello, &remote, "dialer", "acceptor")
        } else {
            (&remote, &hello, "acceptor", "dialer")
        };
        let signature = keys
            .sign(NAMESPACE, &transcript(dialer, acceptor, our_role)?)
            .map_err(|_| error("cannot sign channel challenge"))?
            .to_pem(ssh_key::LineEnding::LF)
            .map_err(|_| error("cannot encode channel proof"))?;
        wire::write(stream, &Proof { signature }).await?;
        let proof: Proof = wire::read(stream, &budget).await?.value;
        verify(
            &member.request.ssh_public_key,
            &transcript(dialer, acceptor, their_role)?,
            &proof.signature,
        )?;
        Ok(remote.server)
    })
    .await?
}

pub(super) struct SessionEnd {
    pub authenticated: bool,
    pub result: io::Result<()>,
}
impl SessionEnd {
    pub fn rejected(error: io::Error) -> Self {
        Self {
            authenticated: false,
            result: Err(error),
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn serve(
    stream: TcpStream,
    config: &NodeConfig,
    keys: &LocalDeviceKeys,
    roster: &Roster,
    expected: Option<&DeviceId>,
    generation: u64,
    commands: mpsc::Sender<Command>,
    inbound: wire::Budget,
    outbound: wire::Budget,
) -> SessionEnd {
    let remote_ip = match stream.peer_addr() {
        Ok(remote) => remote.ip(),
        Err(error) => return SessionEnd::rejected(error),
    };
    serve_stream(
        stream,
        &config.network.id,
        &config.server_id,
        keys,
        roster,
        remote_ip,
        expected,
        generation,
        commands,
        inbound,
        outbound,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn serve_stream<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    network: &NetworkId,
    local: &DeviceId,
    keys: &LocalDeviceKeys,
    roster: &Roster,
    remote_ip: IpAddr,
    expected: Option<&DeviceId>,
    generation: u64,
    commands: mpsc::Sender<Command>,
    inbound: wire::Budget,
    outbound: wire::Budget,
) -> SessionEnd {
    let peer = match authenticate(
        &mut stream,
        network,
        local,
        keys,
        roster,
        remote_ip,
        expected,
    )
    .await
    {
        Ok(peer) => peer,
        Err(error) => return SessionEnd::rejected(error),
    };
    // The authenticated peer may already be committing events while local broker admission waits.
    // From this point onward, even admission timeout is a delivery gap; bad signatures are not.
    let result = async {
        let (sender, receiver) = wire::queue(outbound);
        let (reply, receive) = oneshot::channel();
        tokio::time::timeout(
            IO_TIMEOUT,
            commands.send(Command::Attach {
                peer: peer.clone(),
                generation,
                sender,
                reply,
            }),
        )
        .await?
        .map_err(|_| error("channel broker stopped"))?;
        if !tokio::time::timeout(IO_TIMEOUT, receive)
            .await?
            .map_err(|_| error("channel broker stopped"))?
        {
            return Err(error("stale channel membership"));
        }
        pump(stream, peer, generation, commands, inbound, receiver).await
    }
    .await;
    SessionEnd {
        authenticated: true,
        result,
    }
}

pub(super) async fn pump<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    peer: DeviceId,
    generation: u64,
    commands: mpsc::Sender<Command>,
    inbound: wire::Budget,
    mut outgoing: mpsc::Receiver<wire::Packet>,
) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let read = async {
        loop {
            let frame = wire::read::<_, Frame>(&mut reader, &inbound).await?;
            if matches!(frame.value, Frame::Keepalive) {
                continue;
            }
            tokio::time::timeout(
                IO_TIMEOUT,
                commands.send(Command::Remote {
                    peer: peer.clone(),
                    generation,
                    frame,
                }),
            )
            .await?
            .map_err(|_| error("channel broker stopped"))?;
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    let write = async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                packet = outgoing.recv() => match packet {
                    Some(packet) => wire::write_bytes(&mut writer, &packet.bytes).await?,
                    None => return Ok::<(), io::Error>(()),
                },
                _ = tick.tick() => wire::write(&mut writer, &Frame::Keepalive).await?,
            }
        }
    };
    // Only terminal completion cancels a read; keepalives cannot discard partial frames.
    tokio::select! { result = read => result, result = write => result }
}
