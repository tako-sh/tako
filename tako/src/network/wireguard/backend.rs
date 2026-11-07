//! The embedded tunnel runs in a detached copy of Tako. Only this helper runs as root;
//! private configuration crosses stdin, never argv or the environment.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv6Addr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use boringtun::device::{DeviceConfig, DeviceHandle};
use daemonize::{Daemonize, Outcome};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::network::{self, Result};
mod dns;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ServerName {
    pub name: String,
    pub address: Ipv6Addr,
}

pub(crate) const HELPER_COMMAND: &str = "__network-tunnel";
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Operation {
    Inspect {
        interface: String,
    },
    Start {
        interface: String,
        configuration: String,
        network: String,
        names: Vec<ServerName>,
    },
    Sync {
        interface: String,
        configuration: String,
        network: String,
        names: Vec<ServerName>,
    },
    Stop {
        interface: String,
        public_key: String,
    },
}

impl Operation {
    fn interface(&self) -> &str {
        match self {
            Self::Inspect { interface }
            | Self::Start { interface, .. }
            | Self::Sync { interface, .. }
            | Self::Stop { interface, .. } => interface,
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Self::Start { configuration, .. } | Self::Sync { configuration, .. } = self {
            configuration.zeroize();
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    public_key: Option<String>,
    error: Option<String>,
}

impl Reply {
    fn from_result(result: Result<Option<String>>) -> Self {
        match result {
            Ok(public_key) => Self {
                public_key,
                error: None,
            },
            Err(error) => Self {
                public_key: None,
                error: Some(error.to_string()),
            },
        }
    }

    fn into_result(self) -> Result<Option<String>> {
        match self.error {
            Some(error) => Err(error.into()),
            None => Ok(self.public_key),
        }
    }
}

pub(super) fn invoke(operation: Operation) -> Result<Option<String>> {
    let executable = std::env::current_exe()?;
    let mut command = if crate::output::is_root() {
        Command::new(executable)
    } else {
        let mut command = Command::new("sudo");
        // Authentication is completed separately, so it cannot be hidden in captured stderr.
        command.args(["-n", "--"]).arg(executable);
        command
    };
    let mut child = command
        .arg(HELPER_COMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let request = Zeroizing::new(serde_json::to_vec(&operation)?);
    let written = child.stdin.take().unwrap().write_all(&request);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Could not manage the local network: {}", detail.trim()).into());
    }
    written?;
    let reply: Reply = serde_json::from_slice(&output.stdout)
        .map_err(|_| "The local network helper returned an invalid response")?;
    reply.into_result()
}

/// Called before the CLI installs signal handlers, telemetry, runtimes, or other threads.
pub(crate) fn helper_main() -> ! {
    let result = helper();
    let reply = Reply::from_result(result);
    let encoded = serde_json::to_vec(&reply).expect("serializing helper reply");
    let _ = std::io::stdout().write_all(&encoded);
    std::process::exit(0);
}

fn helper() -> Result<Option<String>> {
    if !crate::output::is_root() {
        return Err("The local network helper requires administrator access".into());
    }
    // BoringTun uses these inherited variables to change ownership or choose a name file.
    // This helper is single-threaded here and owns all of those choices itself.
    unsafe {
        libc::umask(0o077);
        for key in ["SUDO_UID", "SUDO_GID", "WG_TUN_NAME_FILE"] {
            std::env::remove_var(key);
        }
    }
    let bytes = read_bounded(std::io::stdin())?;
    let operation: Operation =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid local network request")?;
    validate_interface(operation.interface())?;
    let directory = runtime_directory()?;
    let lock = network::private_file(
        &directory.join(format!("{}.lock", operation.interface())),
        true,
    )?;
    lock.lock()?;
    let name_path = directory.join(format!("{}.name", operation.interface()));
    match &operation {
        Operation::Inspect { interface } => {
            if let Some(actual) = active_interface(&name_path, interface)? {
                return Ok(Some(public_key(&actual)?));
            }
            reject_unmanaged_interface(interface)?;
            Ok(None)
        }
        Operation::Start {
            interface,
            configuration,
            network,
            names,
        } => {
            let config = Configuration::parse(configuration)?;
            dns::validate(network, interface, names, &config)?;
            dns::preflight()?;
            if active_interface(&name_path, interface)?.is_some() {
                return Err("The network interface is already active".into());
            }
            reject_unmanaged_interface(interface)?;
            start(interface, config, name_path, lock, network, names)
        }
        Operation::Sync {
            interface,
            configuration,
            network,
            names,
        } => {
            let config = Configuration::parse(configuration)?;
            dns::validate(network, interface, names, &config)?;
            let actual = active_interface(&name_path, interface)?
                .ok_or("The network interface is no longer active")?;
            require_owner(&actual, &config.public_key())?;
            // BoringTun cannot update an existing peer in place. Preserve live
            // handshakes when the configuration already matches.
            if !config.matches_peers(&uapi(&actual, "get=1\n\n")?) {
                uapi(&actual, &config.uapi())?;
            }
            dns::install(network, interface, &actual, names, &config)?;
            Ok(None)
        }
        Operation::Stop {
            interface,
            public_key,
        } => {
            if let Some(actual) = active_interface(&name_path, interface)? {
                require_owner(&actual, public_key)?;
                dns::cleanup(interface, Some(&actual))?;
                // BoringTun watches its control socket and exits when it is removed.
                // Closing the TUN device removes its addresses and interface-bound routes.
                fs::remove_file(socket_path(&actual))?;
                let deadline = Instant::now() + Duration::from_secs(10);
                while interface_exists(&actual)? {
                    if Instant::now() >= deadline {
                        return Err(
                            "Timed out waiting for the local network interface to stop".into()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                remove_name(&name_path)?;
            }
            Ok(None)
        }
    }
}

fn runtime_directory() -> Result<PathBuf> {
    // /var/run is a system symlink on both supported platforms. Resolve that trusted
    // parent before using the no-symlink private-file helpers for Tako's own state.
    let directory = fs::canonicalize("/var/run")?.join("tako-network");
    network::private_directory(&directory)?;
    Ok(directory)
}

fn validate_interface(interface: &str) -> Result<()> {
    if interface.len() != 14
        || !interface.starts_with("tako")
        || !interface[4..]
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("Invalid Tako network interface name".into());
    }
    Ok(())
}

fn interface_exists(interface: &str) -> Result<bool> {
    let name = CString::new(interface)?;
    Ok(unsafe { libc::if_nametoindex(name.as_ptr()) } != 0)
}

fn active_interface(path: &Path, logical: &str) -> Result<Option<String>> {
    let file = match network::private_file(path, false) {
        Ok(file) => file,
        Err(error) if is_not_found(error.as_ref()) => return Ok(None),
        Err(error) => return Err(error),
    };
    let value = read_bounded(file)?;
    let actual = std::str::from_utf8(&value)?.trim();
    #[cfg(target_os = "linux")]
    let valid = actual == logical;
    #[cfg(target_os = "macos")]
    let valid = actual
        .strip_prefix("utun")
        .is_some_and(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()));
    #[cfg(target_os = "macos")]
    let _ = logical;
    if !valid {
        return Err("Invalid saved network interface name".into());
    }
    if !interface_exists(actual)? {
        dns::cleanup(logical, None)?;
        remove_name(path)?;
        return Ok(None);
    }
    Ok(Some(actual.to_string()))
}

fn is_not_found(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn remove_name(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_unmanaged_interface(logical: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    let exists = interface_exists(logical)?;
    #[cfg(target_os = "macos")]
    let exists = fs::read_to_string(format!("/var/run/wireguard/{logical}.name"))
        .ok()
        .is_some_and(|name| interface_exists(name.trim()).unwrap_or(true));
    if exists {
        return Err("An older or unmanaged connection already uses this network. Disconnect it with the previous Tako version before reconnecting.".into());
    }
    Ok(())
}

fn start(
    logical: &str,
    config: Configuration,
    name_path: PathBuf,
    lock: File,
    network: &str,
    names: &[ServerName],
) -> Result<Option<String>> {
    let (mut parent, mut child) = UnixStream::pair()?;
    parent.set_read_timeout(Some(Duration::from_secs(30)))?;
    // No threads exist in the helper yet. Forking happens before BoringTun starts workers.
    match Daemonize::new()
        .working_directory("/")
        .umask(0o077)
        .execute()
    {
        Outcome::Parent(result) => {
            result?;
            drop(child);
            let bytes = read_bounded(&mut parent)?;
            let reply: Reply = serde_json::from_slice(&bytes)
                .map_err(|_| "The embedded tunnel did not report readiness")?;
            reply.into_result()
        }
        Outcome::Child(result) => {
            drop(parent);
            let running = result
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })
                .and_then(|_| {
                    let device = start_device(logical, &config, &name_path)?;
                    let actual = active_interface(&name_path, logical)?
                        .ok_or("The tunnel stopped during setup")?;
                    dns::install(network, logical, &actual, names, &config)?;
                    Ok(device)
                });
            match running {
                Ok(mut device) => {
                    let reply = Reply::from_result(Ok(None));
                    let notified = serde_json::to_writer(&mut child, &reply).is_ok();
                    drop(child);
                    drop(lock);
                    drop(config);
                    if notified {
                        device.wait();
                    }
                    let _ = dns::cleanup(logical, None);
                    drop(device);
                    std::process::exit(0);
                }
                Err(error) => {
                    let _ = dns::cleanup(logical, None);
                    let _ = remove_name(&name_path);
                    let _ = serde_json::to_writer(&mut child, &Reply::from_result(Err(error)));
                    std::process::exit(1);
                }
            }
        }
    }
}

fn start_device(logical: &str, config: &Configuration, name_path: &Path) -> Result<DeviceHandle> {
    let socket_directory = fs::canonicalize("/var/run")?.join("wireguard");
    if !socket_directory.exists() {
        fs::create_dir(&socket_directory)?;
    }
    let metadata = fs::symlink_metadata(&socket_directory)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(
            "WireGuard's runtime directory must be owned by root and not writable by others".into(),
        );
    }
    #[cfg(target_os = "macos")]
    let requested = {
        // This is still the single-threaded child, before creating the engine.
        unsafe {
            std::env::set_var("WG_TUN_NAME_FILE", name_path);
        }
        "utun"
    };
    #[cfg(target_os = "linux")]
    let requested = logical;
    let device = DeviceHandle::new(requested, DeviceConfig::default())
        .map_err(|error| format!("Could not start the embedded WireGuard tunnel: {error}"))?;
    #[cfg(target_os = "linux")]
    network::write_bytes(name_path, logical.as_bytes(), true)?;
    let actual =
        active_interface(name_path, logical)?.ok_or("The tunnel did not create an interface")?;
    uapi(&actual, &config.uapi())?;
    require_owner(&actual, &config.public_key())?;
    configure_routes(&actual, config)?;
    Ok(device)
}

fn socket_path(actual: &str) -> PathBuf {
    Path::new("/var/run/wireguard").join(format!("{actual}.sock"))
}

fn uapi(actual: &str, request: &str) -> Result<Zeroizing<String>> {
    let path = socket_path(actual);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        return Err("The WireGuard control socket is not private to root".into());
    }
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    stream.write_all(request.as_bytes())?;
    read_uapi(stream)
}

fn read_uapi(stream: impl Read) -> Result<Zeroizing<String>> {
    let mut reader = BufReader::new(stream.take(MAX_MESSAGE as u64 + 1));
    let mut response = Zeroizing::new(String::new());
    let mut errno = None;
    loop {
        let mut line = Zeroizing::new(String::new());
        if reader.read_line(&mut line)? == 0 {
            return Err("The embedded WireGuard engine returned an incomplete response".into());
        }
        if response.len() + line.len() > MAX_MESSAGE {
            return Err("The WireGuard response is too large".into());
        }
        if line.as_str() == "\n" {
            break;
        }
        if let Some(value) = line.trim_end().strip_prefix("errno=") {
            errno = Some(
                value
                    .parse::<i32>()
                    .map_err(|_| "Invalid WireGuard response status")?,
            );
        }
        response.push_str(&line);
    }
    match errno {
        Some(0) => Ok(response),
        Some(code) => {
            Err(format!("The embedded WireGuard engine rejected the request (errno {code})").into())
        }
        None => Err("The embedded WireGuard engine omitted its response status".into()),
    }
}

fn public_key(actual: &str) -> Result<String> {
    let response = uapi(actual, "get=1\n\n")?;
    let key = response
        .lines()
        .find_map(|line| line.strip_prefix("own_public_key="))
        .ok_or("The embedded WireGuard engine did not report its identity")?;
    let bytes = hex::decode(key).map_err(|_| "Invalid WireGuard public key")?;
    if bytes.len() != 32 {
        return Err("Invalid WireGuard public key".into());
    }
    Ok(STANDARD.encode(bytes))
}

fn require_owner(actual: &str, expected: &str) -> Result<()> {
    if public_key(actual)? != expected {
        return Err("Refusing to change a different WireGuard connection".into());
    }
    Ok(())
}

fn read_bounded(reader: impl Read) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .take(MAX_MESSAGE as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MESSAGE {
        return Err("The local network message exceeds 4 MiB".into());
    }
    Ok(bytes)
}

struct Configuration {
    private_key: Zeroizing<[u8; 32]>,
    address: Ipv6Addr,
    peers: Vec<Peer>,
}

struct Peer {
    key: [u8; 32],
    address: Ipv6Addr,
    endpoint: SocketAddr,
}

impl Configuration {
    fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        if lines.next() != Some("[Interface]") {
            return Err("Invalid WireGuard configuration".into());
        }
        let private_key = Zeroizing::new(parse_key(value(lines.next(), "PrivateKey = ")?)?);
        let address = parse_address(value(lines.next(), "Address = ")?)?;
        let mut peers = Vec::new();
        while let Some(line) = lines.next() {
            if !line.is_empty() || lines.next() != Some("[Peer]") {
                return Err("Unexpected WireGuard configuration directive".into());
            }
            let key = parse_key(value(lines.next(), "PublicKey = ")?)?;
            let address = parse_address(value(lines.next(), "AllowedIPs = ")?)?;
            let endpoint = value(lines.next(), "Endpoint = ")?
                .parse()
                .map_err(|_| "Invalid WireGuard endpoint")?;
            if lines.next() != Some("PersistentKeepalive = 25") {
                return Err("Unexpected WireGuard keepalive".into());
            }
            peers.push(Peer {
                key,
                address,
                endpoint,
            });
        }
        if peers.is_empty() {
            return Err("No servers are configured for this network".into());
        }
        Ok(Self {
            private_key,
            address,
            peers,
        })
    }

    fn public_key(&self) -> String {
        let secret = boringtun::x25519::StaticSecret::from(*self.private_key);
        STANDARD.encode(boringtun::x25519::PublicKey::from(&secret).as_bytes())
    }

    fn uapi(&self) -> Zeroizing<String> {
        let mut value = Zeroizing::new(format!(
            "set=1\nprivate_key={}\nreplace_peers=true\n",
            hex::encode(self.private_key.as_slice()),
        ));
        for peer in &self.peers {
            value.push_str(&format!(
                "public_key={}\nreplace_allowed_ips=true\nallowed_ip={}/128\nendpoint={}\npersistent_keepalive_interval=25\n",
                hex::encode(peer.key), peer.address, peer.endpoint,
            ));
        }
        value.push('\n');
        value
    }

    fn matches_peers(&self, current: &str) -> bool {
        let sections: Vec<_> = current.split("\npublic_key=").skip(1).collect();
        sections.len() == self.peers.len()
            && self.peers.iter().all(|peer| {
                let key = hex::encode(peer.key);
                sections.iter().any(|section| {
                    let mut lines = section.lines();
                    if lines.next() != Some(key.as_str()) {
                        return false;
                    }
                    let settings: Vec<_> = lines.filter_map(|line| line.split_once('=')).collect();
                    let values = |name| {
                        settings
                            .iter()
                            .filter(move |(field, _)| *field == name)
                            .map(|(_, value)| *value)
                            .collect::<Vec<_>>()
                    };
                    values("allowed_ip") == [format!("{}/128", peer.address)]
                        && values("endpoint") == [peer.endpoint.to_string()]
                        && values("persistent_keepalive_interval") == ["25"]
                        && values("preshared_key").is_empty()
                })
            })
    }
}

fn value<'a>(line: Option<&'a str>, prefix: &str) -> Result<&'a str> {
    line.and_then(|line| line.strip_prefix(prefix))
        .ok_or_else(|| "Invalid WireGuard configuration".into())
}

fn parse_key(value: &str) -> Result<[u8; 32]> {
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

fn parse_address(value: &str) -> Result<Ipv6Addr> {
    value
        .strip_suffix("/128")
        .ok_or("WireGuard routes must be single hosts")?
        .parse()
        .map_err(|_| "Invalid WireGuard address".into())
}

fn configure_routes(actual: &str, config: &Configuration) -> Result<()> {
    let address = format!("{}/128", config.address);
    #[cfg(target_os = "linux")]
    {
        run_tool("ip", &["address", "add", &address, "dev", actual])?;
        run_tool("ip", &["link", "set", "dev", actual, "mtu", "1280", "up"])?;
        for peer in &config.peers {
            run_tool(
                "ip",
                &[
                    "-6",
                    "route",
                    "add",
                    &format!("{}/128", peer.address),
                    "dev",
                    actual,
                ],
            )?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        run_tool("/sbin/ifconfig", &[actual, "inet6", &address, "alias"])?;
        run_tool("/sbin/ifconfig", &[actual, "mtu", "1280", "up"])?;
        for peer in &config.peers {
            run_tool(
                "/sbin/route",
                &[
                    "-q",
                    "-n",
                    "add",
                    "-inet6",
                    &format!("{}/128", peer.address),
                    "-interface",
                    actual,
                ],
            )?;
        }
    }
    Ok(())
}

fn run_tool(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Could not run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not configure the local network: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapi_requires_explicit_success_and_complete_response() {
        assert!(read_uapi(&b"errno=0\n\n"[..]).is_ok());
        for response in [
            "\n",
            "errno=0\n",
            "errno=5\n\n",
            "errno=bad\n\n",
            "own_public_key=abc\n\n",
        ] {
            assert!(read_uapi(response.as_bytes()).is_err(), "{response:?}");
        }
    }

    #[test]
    fn parses_only_host_routes_and_never_accepts_hooks() {
        let fixture = crate::network::tests::fixture();
        let text = super::super::configuration(&fixture.profile, &fixture.keys).unwrap();
        let config = Configuration::parse(&text).unwrap();
        assert_eq!(
            config.public_key(),
            fixture.keys.wg_public_key().unwrap().to_string()
        );
        assert_eq!(config.peers.len(), 1);
        let request = config.uapi();
        assert!(request.starts_with("set=1\nprivate_key="));
        assert!(request.contains("replace_peers=true\n"));
        assert!(request.contains("persistent_keepalive_interval=25\n"));
        assert!(request.ends_with("\n\n"));
        assert!(Configuration::parse(&text.replace("/128", "/0")).is_err());
        assert!(Configuration::parse(&format!("{text}PostUp = unsafe\n")).is_err());
    }

    #[test]
    fn invalid_requests_cannot_escape_the_runtime_directory() {
        for name in [
            "../other",
            "utun10",
            "tako000000000/",
            "tako000000000\n",
            "tako000000000g",
        ] {
            assert!(validate_interface(name).is_err());
        }
        assert!(validate_interface("tako0123456789").is_ok());
    }
}
