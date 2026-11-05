use std::{
    error::Error,
    io::{Read, Write},
    path::Path,
};
use tako_network::{BootstrapRequest, BootstrapResponse, MAX_BOOTSTRAP_BYTES};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn run(data_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        exchange(
            std::io::stdin().lock(),
            std::io::stdout().lock(),
            |request| unix::handle(data_dir, request, &unix::Host::production()),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        Err("native bootstrap requires Unix".into())
    }
}

fn exchange(
    input: impl Read,
    mut output: impl Write,
    handle: impl FnOnce(BootstrapRequest) -> Result<BootstrapResponse>,
) -> Result<()> {
    let mut bytes = Vec::new();
    input
        .take(MAX_BOOTSTRAP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BOOTSTRAP_BYTES {
        return Err("bootstrap request exceeds 4 MiB".into());
    }
    let request = serde_json::from_slice(&bytes).map_err(|_| "invalid bootstrap request JSON")?;
    let response = serde_json::to_vec(&handle(request)?)?;
    if response.len() > MAX_BOOTSTRAP_BYTES {
        return Err("bootstrap response exceeds 4 MiB".into());
    }
    output.write_all(&response)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

#[cfg(unix)]
mod unix {
    use super::*;
    use openssl::{
        hash::{Hasher, MessageDigest},
        rand::rand_bytes,
    };
    use std::{
        fs::{self, File, OpenOptions},
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        path::{Component, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };
    use tako_network::{
        DeviceKind, MemberKind, NetworkId, NodeConfig, NodeFiles, RegistrySnapshot, ServerIdentity,
        SshPublicKey,
        api::SignedRequest,
        node_config::{MAX_NODE_CONFIG_BYTES, NODE_CONFIG_FILE, protected::Directory},
    };

    const TOKEN_FILE: &str = "bootstrap-api-token";
    const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;

    pub(super) struct Host<'a> {
        pub host_key: &'a Path,
        pub binary: &'a Path,
        pub receipt: &'a Path,
        pub owner: u32,
    }

    impl Host<'static> {
        pub(super) fn production() -> Self {
            Self {
                host_key: Path::new("/etc/ssh/ssh_host_ed25519_key.pub"),
                binary: Path::new("/usr/local/libexec/tako/corrosion"),
                receipt: Path::new("/usr/local/libexec/tako/corrosion.sha256"),
                owner: 0,
            }
        }
    }

    pub(super) fn handle(
        data_dir: &Path,
        request: BootstrapRequest,
        host: &Host<'_>,
    ) -> Result<BootstrapResponse> {
        let data = Directory::open(data_dir, false)?;
        if fs::symlink_metadata(data_dir)?.uid() != unsafe { libc::geteuid() } {
            return Err("bootstrap must run as the server data owner".into());
        }
        let _lock = data.lock("bootstrap.lock")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        match request {
            BootstrapRequest::Prepare {
                network,
                name,
                wireguard_endpoint,
                ssh_port,
                https_port,
            } => {
                if wireguard_endpoint.port() < 1024 || wireguard_endpoint.port() == 8787 {
                    return Err("invalid WireGuard listen port".into());
                }
                if data.exists(NODE_CONFIG_FILE)? {
                    let installed = NodeConfig::load(data_dir)?;
                    if installed.network.id != network {
                        return Err("server already belongs to a different network".into());
                    }
                }
                let networks = data_dir.join("networks");
                match fs::DirBuilder::new().mode(0o700).create(&networks) {
                    Ok(()) => File::open(data_dir)?.sync_all()?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(error) => return Err(error.into()),
                }
                Directory::open(&networks, true)?;
                let identity = ServerIdentity::prepare(
                    networks.join(network.to_string()),
                    network,
                    name,
                    wireguard_endpoint,
                    read_host_key(host)?,
                    ssh_port,
                    https_port,
                )?;
                Ok(BootstrapResponse::Prepared {
                    request: identity.request().clone(),
                    server: identity.server_info().clone(),
                })
            }
            BootstrapRequest::Answer { network, challenge } => {
                let identity = load_identity(data_dir, &network)?;
                Ok(BootstrapResponse::Proof {
                    proof: identity.answer_challenge(&challenge, now)?,
                })
            }
            BootstrapRequest::Sign {
                network,
                server,
                request,
            } => {
                let identity = load_identity(data_dir, &network)?;
                Ok(BootstrapResponse::Signed {
                    request: SignedRequest::sign(
                        identity.keys(),
                        network,
                        server,
                        DeviceKind::Server,
                        now,
                        request,
                    )?,
                })
            }
            BootstrapRequest::Install { snapshot } => install(data_dir, &data, snapshot, host, now),
        }
    }

    fn load_identity(data_dir: &Path, network: &NetworkId) -> Result<ServerIdentity> {
        let path = data_dir.join("networks").join(network.to_string());
        Directory::open(&path, true)?;
        let identity = ServerIdentity::renew(path)?;
        if identity.request().network_id.as_ref() != Some(network) {
            return Err("prepared identity belongs to a different network".into());
        }
        Ok(identity)
    }

    fn install(
        data_dir: &Path,
        data: &Directory,
        snapshot: RegistrySnapshot,
        host: &Host<'_>,
        now: u64,
    ) -> Result<BootstrapResponse> {
        let identity = load_identity(data_dir, &snapshot.network.id)?;
        let device = identity.request().device_id_for(&snapshot.network.id)?;
        let effective = snapshot.effective_members(now)?;
        let member = effective
            .get(&device)
            .ok_or("prepared server is not a current member")?;
        if member.request != *identity.request()
            || member.kind != MemberKind::Server(identity.server_info().clone())
            || !member.permissions.server_admin
        {
            return Err(
                "admission does not match the prepared server identity and permissions".into(),
            );
        }
        if read_host_key(host)? != identity.server_info().ssh_host_key {
            return Err("SSH host key changed after preparation".into());
        }
        let config = NodeConfig {
            version: 1,
            network: snapshot.network.clone(),
            server_id: device.clone(),
            bootstrap: snapshot,
            files: NodeFiles {
                keys: filename(identity.keys_path())?,
                https_certificate: filename(&identity.tls_paths().fullchain)?,
                https_private_key: filename(&identity.tls_paths().private_key)?,
                gossip_ca_certificate: filename(&identity.gossip_paths().ca_certificate)?,
                gossip_certificate: filename(&identity.gossip_paths().fullchain)?,
                gossip_private_key: filename(&identity.gossip_paths().private_key)?,
                api_token: TOKEN_FILE.into(),
            },
            corrosion_binary_sha256: verify_binary(host)?,
            wireguard_listen_port: identity.server_info().wireguard_endpoint.port(),
        };
        config.validate()?;
        let directory = Directory::open(&config.network_dir(data_dir), true)?;
        for name in config
            .files
            .names()
            .into_iter()
            .filter(|name| *name != TOKEN_FILE)
        {
            directory.open_file(name)?;
        }
        let existing = if data.exists(NODE_CONFIG_FILE)? {
            let bytes = data.read(NODE_CONFIG_FILE, MAX_NODE_CONFIG_BYTES)?;
            let mut previous: NodeConfig = serde_json::from_slice(&bytes)?;
            previous.validate()?;
            if !previous
                .bootstrap
                .members
                .iter()
                .all(|member| config.bootstrap.members.contains(member))
                || !previous
                    .bootstrap
                    .revocations
                    .iter()
                    .all(|revocation| config.bootstrap.revocations.contains(revocation))
            {
                return Err(
                    "installed network history is missing or changed in the enrollment snapshot"
                        .into(),
                );
            }
            // Installation retries may observe a newer registry. The running
            // agent owns that registry; leave its original bootstrap untouched.
            previous.bootstrap = config.bootstrap.clone();
            if serde_json::to_value(previous)? != serde_json::to_value(&config)? {
                return Err("refusing to replace an existing native network configuration".into());
            }
            true
        } else {
            false
        };
        if directory.exists(TOKEN_FILE)? {
            let token = directory.read(TOKEN_FILE, 64)?;
            if token.len() != 64 || !token.iter().all(u8::is_ascii_hexdigit) {
                return Err("invalid local Corrosion API token".into());
            }
        } else {
            if existing {
                return Err("installed network is missing its API token".into());
            }
            let mut random = [0u8; 32];
            rand_bytes(&mut random)?;
            directory.write(TOKEN_FILE, hex::encode(random).as_bytes())?;
        }
        if !existing {
            data.write(NODE_CONFIG_FILE, &serde_json::to_vec(&config)?)?;
        }
        Ok(BootstrapResponse::Installed {
            network: config.network.id,
            device,
        })
    }

    fn filename(path: &Path) -> Result<String> {
        Ok(path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid prepared filename")?
            .to_owned())
    }

    fn read_host_key(host: &Host<'_>) -> Result<SshPublicKey> {
        let mut bytes = Vec::new();
        trusted_file(host.host_key, host.owner)?
            .take(4097)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err("SSH host public key exceeds size limit".into());
        }
        let text = std::str::from_utf8(&bytes)?;
        if text.trim().lines().count() != 1 {
            return Err("expected one SSH host public key".into());
        }
        let mut fields = text.split_whitespace();
        let algorithm = fields.next().ok_or("missing SSH host key algorithm")?;
        let key = fields.next().ok_or("missing SSH host public key")?;
        Ok(format!("{algorithm} {key}").parse()?)
    }

    fn verify_binary(host: &Host<'_>) -> Result<String> {
        let mut bytes = Vec::new();
        trusted_file(host.receipt, host.owner)?
            .take(66)
            .read_to_end(&mut bytes)?;
        let receipt = std::str::from_utf8(&bytes)?
            .strip_suffix('\n')
            .unwrap_or(std::str::from_utf8(&bytes)?);
        if receipt.len() != 64
            || !receipt
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid installed Corrosion digest receipt".into());
        }
        let mut binary = trusted_file(host.binary, host.owner)?;
        if binary.metadata()?.mode() & 0o111 == 0 {
            return Err("Corrosion binary is not executable".into());
        }
        let mut hasher = Hasher::new(MessageDigest::sha256())?;
        let mut buffer = [0u8; 64 * 1024];
        let mut total = 0;
        loop {
            let count = binary.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_BINARY_BYTES {
                return Err("Corrosion binary exceeds size limit".into());
            }
            hasher.update(&buffer[..count])?;
        }
        if total == 0 || hex::encode(hasher.finish()?) != receipt {
            return Err("Corrosion executable does not match its receipt".into());
        }
        Ok(receipt.to_owned())
    }

    fn trusted_file(path: &Path, owner: u32) -> Result<File> {
        if !path.is_absolute() {
            return Err("trusted host paths must be absolute".into());
        }
        let mut ancestor = PathBuf::from("/");
        for component in path
            .parent()
            .ok_or("missing host file parent")?
            .components()
        {
            match component {
                Component::RootDir => continue,
                Component::Normal(name) => ancestor.push(name),
                _ => return Err("host path traversal is forbidden".into()),
            }
            let metadata = fs::symlink_metadata(&ancestor)?;
            if !metadata.is_dir()
                || (metadata.uid() != 0 && metadata.uid() != owner)
                || (metadata.mode() & 0o022 != 0
                    && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0))
            {
                return Err("unsafe trusted host directory".into());
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != owner
            || metadata.nlink() != 1
            || metadata.mode() & 0o022 != 0
        {
            return Err("unsafe trusted host file".into());
        }
        Ok(file)
    }

    #[cfg(test)]
    mod tests {
        include!("network_bootstrap/tests.rs");
    }
}
