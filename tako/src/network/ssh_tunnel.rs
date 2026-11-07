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

pub(crate) fn proxy_command(executable: &Path, network: &str, server: &str) -> Result<String> {
    let executable = executable
        .to_str()
        .ok_or("Tako's executable path must be UTF-8")?;
    // OpenSSH expands percent tokens before invoking the shell, even inside quotes.
    Ok(format!(
        "exec {}",
        [executable, PROXY_COMMAND, network, server]
            .map(|arg| { crate::shell::shell_single_quote(&arg.replace('%', "%%")) })
            .join(" ")
    ))
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
        let alias = super::resolve_network(Some(network), None)?;
        let profile = super::load_profile(&alias)?;
        let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
        let config = configuration(&profile, &keys, &server.parse()?)?;
        let proxies = super::ssh_session::lock(&alias, "ssh-proxies.lock")?;
        proxies.lock_shared()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            // An assigned local address means connect already owns this device's
            // WireGuard transport. Reuse it instead of stealing the peer endpoint.
            if std::net::UdpSocket::bind((config.source, 0)).is_ok() {
                let mut stream = tokio::time::timeout(
                    Duration::from_secs(15),
                    tokio::net::TcpStream::connect((config.destination, config.port)),
                )
                .await??;
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

struct TunnelConfig {
    private_key: Zeroizing<[u8; 32]>,
    peer_key: [u8; 32],
    endpoint: SocketAddr,
    source: Ipv6Addr,
    destination: Ipv6Addr,
    port: u16,
}

fn decode_key(value: &str) -> Result<[u8; 32]> {
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
    let udp = UdpSocket::bind(if config.endpoint.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;
    udp.connect(config.endpoint).await?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random)?;
    let mut tunnel = Tunn::new(
        StaticSecret::from(*config.private_key),
        PublicKey::from(config.peer_key),
        None,
        Some(25),
        u32::from_le_bytes(random[..4].try_into().unwrap()) & 0xffffff,
        None,
    );
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
    let mut datagram = vec![0; 65536];
    let mut packet = vec![0; 65536];
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let now = StackInstant::from_millis(started.elapsed().as_millis() as i64);
        interface.poll(now, &mut device, &mut sockets);
        let socket = sockets.get_mut::<tcp::Socket>(handle);
        established |= socket.state() == tcp::State::Established;
        if !established && started.elapsed() > Duration::from_secs(15) {
            return Err("Timed out reaching SSH over WireGuard. Check the server's UDP endpoint and private SSH listener.".into());
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
        if socket.can_recv()
            && let Ok(permit) = outgoing.try_reserve()
        {
            let mut bytes = vec![0; 8192];
            let size = socket
                .recv_slice(&mut bytes)
                .map_err(|_| "Could not read private SSH data")?;
            bytes.truncate(size);
            permit.send(bytes);
        }
        if !socket.may_recv() && !socket.can_recv() && established {
            socket.close();
        }
        if socket.state() == tcp::State::Closed {
            if !established {
                return Err("The private SSH server refused the connection".into());
            }
            break;
        }
        while let Some(ip) = device.outgoing.pop_front() {
            if let TunnResult::WriteToNetwork(bytes) = tunnel.encapsulate(&ip, &mut packet) {
                udp.send(bytes).await?;
            }
        }
        tokio::select! {
            read = udp.recv(&mut datagram) => {
                let size = read.map_err(|error| format!("WireGuard endpoint {}: {error}", config.endpoint))?;
                let mut received = &datagram[..size];
                loop {
                    match tunnel.decapsulate(Some(config.endpoint.ip()), received, &mut packet) {
                        TunnResult::WriteToNetwork(bytes) => { udp.send(bytes).await?; }
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
                    udp.send(bytes).await?;
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

    #[test]
    fn proxy_arguments_survive_shell_and_ssh_expansion() {
        let command =
            proxy_command(Path::new("/tmp/my tako's %h"), "work '$HOME' %p", "server").unwrap();
        assert!(command.contains("%%h"));
        assert!(command.contains("%%p"));
        assert!(command.contains("'\\''"));
        assert!(command.starts_with("exec "));
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
        let (mut client, input) = tokio::io::duplex(16384);
        let (output, mut received) = tokio::io::duplex(16384);
        let expected: Vec<_> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        let sent = expected.clone();
        let sender = tokio::spawn(async move {
            client.write_all(&sent).await.unwrap();
            client.shutdown().await.unwrap();
        });
        let receiver = tokio::spawn(async move {
            let mut result = vec![];
            received.read_to_end(&mut result).await.unwrap();
            result
        });
        tokio::time::timeout(Duration::from_secs(30), forward(config, input, output))
            .await
            .unwrap()
            .unwrap();
        sender.await.unwrap();
        assert_eq!(receiver.await.unwrap(), expected);
    }
}
