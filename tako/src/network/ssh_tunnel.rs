//! A single private TCP connection over WireGuard, without a TUN device or routes.

use std::collections::VecDeque;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use boringtun::{
    noise::{Tunn, TunnResult},
    x25519::{PublicKey, StaticSecret},
};
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::tcp,
    time::Instant as StackInstant,
    wire::{HardwareAddress, IpAddress, IpCidr},
};
use tako_network::{DeviceId, LocalDeviceKeys, MemberKind};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UdpSocket,
    sync::mpsc,
    task::JoinHandle,
};
use zeroize::Zeroizing;

use super::{Profile, Result};

pub(crate) const PROXY_COMMAND: &str = "__ssh-proxy";

/// Two handshake attempts: BoringTun retries an unanswered initiation after
/// five seconds. This stays below OpenSSH's 15-second banner timeout so the
/// specific UDP diagnosis prints instead of a generic SSH failure.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const SSH_TIMEOUT: Duration = Duration::from_secs(15);

/// Result of a WireGuard handshake with a server endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reachability {
    /// The server answered the handshake. RTT is BoringTun's measurement.
    Reachable { rtt: Option<Duration> },
    /// Nothing came back. Servers silently drop handshakes they cannot
    /// authenticate, so a firewall and an unknown peer look the same.
    NoReply,
    /// The host answered with ICMP port unreachable.
    Refused,
}

pub(crate) fn no_reply_message(endpoint: SocketAddr, waited: Duration) -> String {
    format!(
        "UDP {port} to {host} isn't getting through: no WireGuard reply in {secs}s. \
         Allow inbound UDP {port} in the server's firewall and provider security group, \
         and check that this network allows outbound UDP. If the port is open, the server \
         may not have applied this device's membership yet.",
        port = endpoint.port(),
        host = endpoint.ip(),
        secs = waited.as_secs(),
    )
}

pub(crate) fn refused_message(endpoint: SocketAddr) -> String {
    format!(
        "UDP {port} to {host} was refused: nothing is listening on that port, or a firewall \
         is rejecting it. Check that tako-server is running, that {endpoint} is the server's \
         WireGuard endpoint, and that inbound UDP {port} is allowed.",
        port = endpoint.port(),
        host = endpoint.ip(),
    )
}

fn udp_error(endpoint: SocketAddr, error: std::io::Error) -> Box<dyn std::error::Error> {
    if error.kind() == std::io::ErrorKind::ConnectionRefused {
        refused_message(endpoint).into()
    } else {
        format!(
            "UDP {} to {} isn't getting through: {error}",
            endpoint.port(),
            endpoint.ip()
        )
        .into()
    }
}

async fn open_tunnel(
    private_key: &Zeroizing<[u8; 32]>,
    peer_key: [u8; 32],
    endpoint: SocketAddr,
    random: [u8; 8],
) -> Result<(UdpSocket, Tunn)> {
    let udp = UdpSocket::bind(if endpoint.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;
    udp.connect(endpoint).await?;
    let tunnel = Tunn::new(
        StaticSecret::from(**private_key),
        PublicKey::from(peer_key),
        None,
        Some(25),
        u32::from_le_bytes(random[..4].try_into().unwrap()) & 0xffffff,
        None,
    );
    Ok((udp, tunnel))
}

/// Completes one WireGuard handshake without sending any tunnel traffic.
///
/// Callers must not probe while another transport for the same device is
/// active: the server roams its peer endpoint to the probe's socket.
pub(crate) async fn probe(
    private_key: &Zeroizing<[u8; 32]>,
    peer_key: [u8; 32],
    endpoint: SocketAddr,
    timeout: Duration,
) -> Result<Reachability> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random)?;
    let (udp, mut tunnel) = open_tunnel(private_key, peer_key, endpoint, random).await?;
    let refused = |error: &std::io::Error| error.kind() == std::io::ErrorKind::ConnectionRefused;
    let mut packet = vec![0; 2048];
    let mut datagram = vec![0; 2048];
    if let TunnResult::WriteToNetwork(bytes) =
        tunnel.format_handshake_initiation(&mut packet, false)
    {
        match udp.send(bytes).await {
            Err(error) if refused(&error) => return Ok(Reachability::Refused),
            result => {
                result.map_err(|error| udp_error(endpoint, error))?;
            }
        }
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            read = udp.recv(&mut datagram) => {
                let size = match read {
                    Ok(size) => size,
                    Err(error) if refused(&error) => return Ok(Reachability::Refused),
                    Err(error) => return Err(udp_error(endpoint, error)),
                };
                let mut received = &datagram[..size];
                while let TunnResult::WriteToNetwork(bytes) =
                    tunnel.decapsulate(Some(endpoint.ip()), received, &mut packet)
                {
                    // Confirm the session so the server does not keep retrying.
                    let _ = udp.send(bytes).await;
                    received = &[];
                }
                if tunnel.time_since_last_handshake().is_some() {
                    let rtt = tunnel.stats().4.map(|ms| Duration::from_millis(ms.into()));
                    return Ok(Reachability::Reachable { rtt });
                }
            }
            _ = tokio::time::sleep_until(deadline) => return Ok(Reachability::NoReply),
            _ = tick.tick() => {
                if let TunnResult::WriteToNetwork(bytes) = tunnel.update_timers(&mut packet) {
                    match udp.send(bytes).await {
                        Err(error) if refused(&error) => return Ok(Reachability::Refused),
                        result => { result.map_err(|error| udp_error(endpoint, error))?; }
                    }
                }
            }
        }
    }
}

pub(crate) fn proxy_command(executable: &Path, network: &str, server: &str) -> Result<String> {
    let executable = executable
        .to_str()
        .ok_or("Tako's executable path must be UTF-8")?;
    // OpenSSH prepends exec and expands percent tokens even inside quotes.
    Ok([executable, PROXY_COMMAND, network, server]
        .map(|arg| crate::shell::shell_single_quote(&arg.replace('%', "%%")))
        .join(" "))
}

pub(crate) fn proxy_main() -> ! {
    let result = (|| -> Result<()> {
        let args = std::env::args_os()
            .skip(2)
            .map(|arg| arg.into_string())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "Invalid private SSH proxy arguments")?;
        let [network, server] = args.as_slice() else {
            return Err("Invalid private SSH proxy arguments".into());
        };
        let alias = super::resolve_network(Some(network))?;
        let profile = super::load_profile(&alias)?;
        let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
        let config = configuration(&profile, &keys, &server.parse()?)?;
        // OpenSSH aliases from `tako ssh config` place their ControlPath here
        // and start the proxy before creating the socket, so ensure it exists.
        super::ssh_session::control_directory()?;
        let proxies = super::ssh_session::lock(&alias, "ssh-proxies.lock")?;
        proxies.lock_shared()?;
        // An assigned local address means connect already owns this device's
        // WireGuard transport. Reuse it instead of stealing the peer endpoint.
        let system = local_address_assigned(config.source)?;
        // Held until this process exits, which ends the transport.
        let _transport = if system {
            None
        } else {
            // Aliases run OpenSSH directly, bypassing `tako ssh`'s guard. The
            // proxy starts before its own master binds, so any live master for
            // this server means a second transport would roam the endpoint.
            // Masters from earlier releases hold no transport lock.
            super::ssh_session::refuse_other_transport(&alias, server, None)?;
            // Clients that start together see no master yet; only one wins.
            Some(super::ssh_session::claim_transport(&alias, server)?)
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            if system {
                let mut stream = tokio::time::timeout(
                    SSH_TIMEOUT,
                    tokio::net::TcpStream::connect((config.destination, config.port)),
                )
                .await
                .map_err(|_| {
                    format!(
                        "Timed out reaching SSH on [{}]:{} through the `tako connect` interface. \
                         Run `tako disconnect`, then retry `tako ssh` to diagnose UDP {} to {}.",
                        config.destination,
                        config.port,
                        config.endpoint.port(),
                        config.endpoint.ip(),
                    )
                })??;
                let mut io = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
                tokio::io::copy_bidirectional(&mut io, &mut stream).await?;
                Ok(())
            } else {
                forward(config, tokio::io::stdin(), tokio::io::stdout()).await
            }
        });
        // Tokio stdin can have a blocked reader when the peer disconnects first.
        runtime.shutdown_background();
        result
    })();
    if let Err(error) = result {
        crate::output::error_stderr(&format!("Private SSH connection failed: {error}"));
        std::process::exit(1);
    }
    std::process::exit(0);
}

#[cfg_attr(test, derive(Clone))]
struct TunnelConfig {
    private_key: Zeroizing<[u8; 32]>,
    peer_key: [u8; 32],
    endpoint: SocketAddr,
    source: Ipv6Addr,
    destination: Ipv6Addr,
    port: u16,
}

pub(crate) fn local_address_assigned(address: Ipv6Addr) -> Result<bool> {
    // Binding is not a reliable test: Linux can allow nonlocal binds globally.
    Ok(nix::ifaddrs::getifaddrs()?.any(|interface| {
        interface
            .flags
            .contains(nix::net::if_::InterfaceFlags::IFF_UP)
            && interface.address.is_some_and(|sockaddr| {
                sockaddr
                    .as_sockaddr_in6()
                    .is_some_and(|ipv6| ipv6.ip() == address)
            })
    }))
}

pub(super) fn decode_key(value: &str) -> Result<[u8; 32]> {
    let bytes = Zeroizing::new(
        STANDARD
            .decode(value)
            .map_err(|_| "Invalid WireGuard key")?,
    );
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| "Invalid WireGuard key length".into())
}

fn configuration(
    profile: &Profile,
    keys: &LocalDeviceKeys,
    server: &DeviceId,
) -> Result<TunnelConfig> {
    profile.validate(keys)?;
    let network = &profile.snapshot().network.id;
    let own = profile.request().device_id_for(network)?;
    if profile
        .snapshot()
        .permissions(&own, super::now()?)?
        .ssh_user
        .is_none()
    {
        return Err("This device has no SSH permission".into());
    }
    let members = super::server_members(profile)?;
    let member = members
        .get(server)
        .ok_or("The SSH server is not an active network member")?;
    let MemberKind::Server(info) = &member.kind else {
        unreachable!()
    };
    Ok(TunnelConfig {
        private_key: Zeroizing::new(decode_key(&keys.wireguard_private_key()?)?),
        peer_key: decode_key(&member.request.wg_public_key.to_string())?,
        endpoint: info.wireguard_endpoint,
        source: network.address(&keys.wg_public_key()?),
        destination: network.address(&member.request.wg_public_key),
        port: info.ssh_port,
    })
}

enum Input {
    Data(Vec<u8>),
    Eof,
    Error(std::io::Error),
}

struct IoTasks {
    reader: JoinHandle<()>,
    writer: JoinHandle<std::io::Result<()>>,
}

impl Drop for IoTasks {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

async fn forward(
    config: TunnelConfig,
    mut input: impl AsyncRead + Unpin + Send + 'static,
    mut output: impl AsyncWrite + Unpin + Send + 'static,
) -> Result<()> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random)?;
    let (udp, mut tunnel) = open_tunnel(
        &config.private_key,
        config.peer_key,
        config.endpoint,
        random,
    )
    .await?;
    let endpoint = config.endpoint;
    let mut device = IpDevice::default();
    let mut stack_config = Config::new(HardwareAddress::Ip);
    stack_config.random_seed = u64::from_le_bytes(random);
    let mut interface = Interface::new(stack_config, &mut device, StackInstant::from_millis(0));
    interface.update_ip_addrs(|addresses| {
        addresses
            .push(IpCidr::new(IpAddress::Ipv6(config.source), 128))
            .unwrap();
    });
    interface
        .routes_mut()
        .add_default_ipv6_route(config.destination)
        .map_err(|_| "Could not configure the private TCP route")?;
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; 64 * 1024]),
        tcp::SocketBuffer::new(vec![0; 64 * 1024]),
    );
    socket.set_timeout(Some(smoltcp::time::Duration::from_secs(60)));
    socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(20)));
    let source_port = 49152 + u16::from_le_bytes(random[..2].try_into().unwrap()) % 16384;
    socket
        .connect(
            interface.context(),
            (IpAddress::Ipv6(config.destination), config.port),
            (IpAddress::Ipv6(config.source), source_port),
        )
        .map_err(|_| "Could not open the private SSH TCP connection")?;
    let mut sockets = SocketSet::new(vec![]);
    let handle = sockets.add(socket);

    // Both directions are bounded. A slow SSH consumer applies TCP backpressure.
    let (incoming, mut input_rx) = mpsc::channel(8);
    let (outgoing, mut output_rx) = mpsc::channel::<Vec<u8>>(8);
    let mut tasks = IoTasks {
        reader: tokio::spawn(async move {
            let mut buffer = [0; 8192];
            loop {
                let message = match input.read(&mut buffer).await {
                    Ok(0) => Input::Eof,
                    Ok(size) => Input::Data(buffer[..size].to_vec()),
                    Err(error) => Input::Error(error),
                };
                let done = !matches!(&message, Input::Data(_));
                if incoming.send(message).await.is_err() || done {
                    break;
                }
            }
        }),
        writer: tokio::spawn(async move {
            while let Some(bytes) = output_rx.recv().await {
                output.write_all(&bytes).await?;
                output.flush().await?;
            }
            output.shutdown().await
        }),
    };
    let started = Instant::now();
    let mut established = false;
    let mut input_closed = false;
    let mut pending = VecDeque::new();
    let mut closing_output = None;
    let mut datagram = vec![0; 65536];
    let mut packet = vec![0; 65536];
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let now = StackInstant::from_millis(started.elapsed().as_millis() as i64);
        interface.poll(now, &mut device, &mut sockets);
        let socket = sockets.get_mut::<tcp::Socket>(handle);
        established |= matches!(
            socket.state(),
            tcp::State::Established | tcp::State::CloseWait
        );
        if !established {
            let handshake = tunnel.time_since_last_handshake().is_some();
            if !handshake && started.elapsed() > HANDSHAKE_TIMEOUT {
                return Err(no_reply_message(endpoint, HANDSHAKE_TIMEOUT).into());
            }
            if started.elapsed() > SSH_TIMEOUT {
                return Err(format!(
                    "WireGuard reached {endpoint}, but the server's private SSH listener on port {} \
                     did not answer. Check that tako-server is running on the server.",
                    config.port
                )
                .into());
            }
        }
        if socket.can_send() && !pending.is_empty() {
            let sent = socket
                .send_slice(pending.make_contiguous())
                .map_err(|_| "The private SSH connection is closed")?;
            pending.drain(..sent);
        }
        if input_closed && pending.is_empty() && socket.may_send() {
            socket.close();
        }
        // smoltcp discards its receive buffer when TIME-WAIT expires, even if
        // stdout is still blocked. Retain that final data (at most the 64 KiB
        // receive window) while continuing to drive ACKs and WireGuard timers.
        if socket.state() == tcp::State::TimeWait && closing_output.is_none() && socket.can_recv() {
            let mut bytes = vec![0; socket.recv_queue()];
            let size = socket
                .recv_slice(&mut bytes)
                .map_err(|_| "Could not read private SSH data")?;
            bytes.truncate(size);
            closing_output = Some(bytes);
        }
        if (closing_output.is_some() || socket.can_recv())
            && let Ok(permit) = outgoing.try_reserve()
        {
            let bytes = if let Some(bytes) = closing_output.take() {
                bytes
            } else {
                let mut bytes = vec![0; 8192];
                let size = socket
                    .recv_slice(&mut bytes)
                    .map_err(|_| "Could not read private SSH data")?;
                bytes.truncate(size);
                bytes
            };
            permit.send(bytes);
        }
        if !socket.may_recv() && !socket.can_recv() && established {
            socket.close();
        }
        if socket.state() == tcp::State::Closed && !socket.can_recv() && closing_output.is_none() {
            if !established {
                return Err("The private SSH server refused the connection".into());
            }
            break;
        }
        while let Some(ip) = device.outgoing.pop_front() {
            if let TunnResult::WriteToNetwork(bytes) = tunnel.encapsulate(&ip, &mut packet) {
                udp.send(bytes)
                    .await
                    .map_err(|error| udp_error(endpoint, error))?;
            }
        }
        tokio::select! {
            read = udp.recv(&mut datagram) => {
                let size = read.map_err(|error| udp_error(endpoint, error))?;
                let mut received = &datagram[..size];
                loop {
                    match tunnel.decapsulate(Some(endpoint.ip()), received, &mut packet) {
                        TunnResult::WriteToNetwork(bytes) => {
                            udp.send(bytes).await.map_err(|error| udp_error(endpoint, error))?;
                        }
                        TunnResult::WriteToTunnelV6(bytes, source) if source == config.destination => {
                            device.incoming.push_back(bytes.to_vec());
                        }
                        _ => break,
                    }
                    received = &[];
                }
            }
            message = input_rx.recv(), if !input_closed && pending.is_empty() => {
                match message {
                    Some(Input::Data(bytes)) => pending.extend(bytes),
                    Some(Input::Error(error)) => return Err(error.into()),
                    Some(Input::Eof) | None => input_closed = true,
                }
            }
            _ = tick.tick() => {
                if let TunnResult::WriteToNetwork(bytes) = tunnel.update_timers(&mut packet) {
                    udp.send(bytes).await.map_err(|error| udp_error(endpoint, error))?;
                }
            }
            result = &mut tasks.writer => {
                result??;
                return Err("SSH closed its proxy output".into());
            }
        }
    }
    drop(outgoing);
    (&mut tasks.writer).await??;
    Ok(())
}

#[derive(Default)]
struct IpDevice {
    incoming: VecDeque<Vec<u8>>,
    outgoing: VecDeque<Vec<u8>>,
}

struct Receive(Vec<u8>);
struct Transmit<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for Receive {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Transmit<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, size: usize, f: F) -> R {
        let mut bytes = vec![0; size];
        let result = f(&mut bytes);
        self.0.push_back(bytes);
        result
    }
}
impl Device for IpDevice {
    type RxToken<'a> = Receive;
    type TxToken<'a> = Transmit<'a>;
    fn receive(&mut self, _: StackInstant) -> Option<(Receive, Transmit<'_>)> {
        self.incoming
            .pop_front()
            .map(|packet| (Receive(packet), Transmit(&mut self.outgoing)))
    }
    fn transmit(&mut self, _: StackInstant) -> Option<Transmit<'_>> {
        (self.outgoing.len() < 64).then_some(Transmit(&mut self.outgoing))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = 1280;
        capabilities.max_burst_size = Some(64);
        capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_pair() -> (Zeroizing<[u8; 32]>, [u8; 32]) {
        let mut private = Zeroizing::new([0u8; 32]);
        getrandom::fill(private.as_mut()).unwrap();
        let public = PublicKey::from(&StaticSecret::from(*private)).to_bytes();
        (private, public)
    }

    #[tokio::test]
    async fn probe_distinguishes_refused_silent_and_answering_endpoints() {
        let (client_private, client_public) = key_pair();
        let (server_private, server_public) = key_pair();

        // Nothing bound: the kernel answers with ICMP port unreachable.
        let closed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let closed_endpoint = closed.local_addr().unwrap();
        drop(closed);
        assert_eq!(
            probe(
                &client_private,
                server_public,
                closed_endpoint,
                Duration::from_secs(2)
            )
            .await
            .unwrap(),
            Reachability::Refused
        );

        // Bound but silent, like a firewall that drops UDP.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            probe(
                &client_private,
                server_public,
                silent.local_addr().unwrap(),
                Duration::from_millis(300)
            )
            .await
            .unwrap(),
            Reachability::NoReply
        );

        // A WireGuard responder that knows the client's key.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let endpoint = server.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let mut tunnel = Tunn::new(
                StaticSecret::from(*server_private),
                PublicKey::from(client_public),
                None,
                None,
                1,
                None,
            );
            let mut datagram = vec![0; 2048];
            let mut packet = vec![0; 2048];
            loop {
                let (size, from) = server.recv_from(&mut datagram).await.unwrap();
                if let TunnResult::WriteToNetwork(bytes) =
                    tunnel.decapsulate(Some(from.ip()), &datagram[..size], &mut packet)
                {
                    server.send_to(bytes, from).await.unwrap();
                }
            }
        });
        let result = probe(
            &client_private,
            server_public,
            endpoint,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Reachability::Reachable { .. }),
            "{result:?}"
        );
        // The wrong server key is silently dropped, as on a real server.
        let (_, stranger) = key_pair();
        assert_eq!(
            probe(
                &client_private,
                stranger,
                endpoint,
                Duration::from_millis(300)
            )
            .await
            .unwrap(),
            Reachability::NoReply
        );
        responder.abort();
    }

    #[test]
    fn unreachable_messages_name_the_port_and_host() {
        let endpoint: SocketAddr = "203.0.113.5:51820".parse().unwrap();
        let message = no_reply_message(endpoint, HANDSHAKE_TIMEOUT);
        assert!(message.starts_with("UDP 51820 to 203.0.113.5 isn't getting through"));
        assert!(message.contains("10s"));
        assert!(refused_message(endpoint).starts_with("UDP 51820 to 203.0.113.5 was refused"));
    }

    #[test]
    fn proxy_arguments_survive_shell_and_ssh_expansion() {
        let command =
            proxy_command(Path::new("/tmp/my tako's %h"), "work '$HOME' %p", "server").unwrap();
        assert!(command.contains("%%h"));
        assert!(command.contains("%%p"));
        assert!(command.contains("'\\''"));
        assert!(command.starts_with("'/tmp/my tako"));
    }

    #[test]
    fn transport_uses_approved_identity_and_private_server_address() {
        let mut fixture = super::super::tests::fixture();
        assert!(configuration(&fixture.profile, &fixture.keys, &fixture.server).is_err());
        let Profile::Active { profile } = &mut fixture.profile else {
            panic!()
        };
        profile.snapshot.members[0].permissions.ssh_user = Some("tako".parse().unwrap());
        let config = configuration(&fixture.profile, &fixture.keys, &fixture.server).unwrap();
        assert_eq!(
            config.source,
            fixture
                .profile
                .snapshot()
                .network
                .id
                .address(&fixture.keys.wg_public_key().unwrap())
        );
        assert_eq!(config.endpoint, "192.0.2.10:51820".parse().unwrap());
        assert!(
            configuration(
                &fixture.profile,
                &LocalDeviceKeys::generate().unwrap(),
                &fixture.server
            )
            .is_err()
        );
        assert!(
            configuration(
                &fixture.profile,
                &fixture.keys,
                &DeviceId::from_keys(
                    &fixture.profile.snapshot().network.id,
                    tako_network::DeviceKind::Laptop,
                    &fixture.keys.ssh_public_key().unwrap(),
                    &fixture.keys.wg_public_key().unwrap(),
                )
                .unwrap()
            )
            .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires an isolated WireGuard echo server; see e2e/cli/rootless-ssh.py"]
    async fn rootless_tcp_roundtrip() {
        assert_ne!(
            unsafe { libc::geteuid() },
            0,
            "exercise the client without administrator privileges"
        );
        let config = TunnelConfig {
            private_key: Zeroizing::new(
                hex::decode("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            peer_key: hex::decode(
                "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a",
            )
            .unwrap()
            .try_into()
            .unwrap(),
            endpoint: std::env::var("TAKO_TEST_WG_ENDPOINT")
                .unwrap()
                .parse()
                .unwrap(),
            source: "fd42::2".parse().unwrap(),
            destination: "fd42::1".parse().unwrap(),
            port: 2222,
        };
        assert!(local_address_assigned(config.destination).unwrap());
        assert!(!local_address_assigned(config.source).unwrap());
        // The fixture enables ip_nonlocal_bind. A successful bind must not make
        // the proxy mistake an unassigned address for an active system tunnel.
        assert!(std::net::UdpSocket::bind((config.source, 0)).is_ok());
        for (size, capacity, delay) in [
            (1024 * 1024, 16384, Duration::ZERO),
            // Keep received bytes buffered past smoltcp's ten-second TIME-WAIT.
            (64 * 1024, 128, Duration::from_secs(12)),
        ] {
            let (mut client, input) = tokio::io::duplex(16384);
            let (output, mut received) = tokio::io::duplex(capacity);
            let expected: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            let sent = expected.clone();
            let sender = tokio::spawn(async move {
                client.write_all(&sent).await.unwrap();
                client.shutdown().await.unwrap();
            });
            let receiver = tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let mut result = vec![];
                received.read_to_end(&mut result).await.unwrap();
                result
            });
            tokio::time::timeout(
                Duration::from_secs(40),
                forward(config.clone(), input, output),
            )
            .await
            .unwrap()
            .unwrap();
            sender.await.unwrap();
            let received = receiver.await.unwrap();
            assert_eq!(received.len(), expected.len(), "TCP output was truncated");
            assert_eq!(received, expected);
        }
    }
}
