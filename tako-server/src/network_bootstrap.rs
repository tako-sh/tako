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

    pub(super) struct Host<'a> {
        pub host_key: &'a Path,
        pub owner: u32,
    }

    impl Host<'static> {
        pub(super) fn production() -> Self {
            Self {
                host_key: Path::new("/etc/ssh/ssh_host_ed25519_key.pub"),
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
                validate_ingress(data_dir)?;
                if wireguard_endpoint.port() < 1024 && wireguard_endpoint.port() != 443 {
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

    fn validate_ingress(data_dir: &Path) -> Result<()> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(data_dir.join("config.json"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.mode() & 0o022 != 0
            || (metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() })
        {
            return Err("unsafe server ingress configuration".into());
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            return Err("server ingress configuration exceeds 64 KiB".into());
        }
        let config: crate::boot::ServerConfigFile = serde_json::from_slice(&bytes)?;
        if config
            .trusted_proxy
            .is_some_and(|proxy| proxy.proxy_protocol)
        {
            return Err("Native network management requires direct TLS; disable mandatory PROXY protocol before enrolling this server".into());
        }
        Ok(())
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
        validate_ingress(data_dir)?;
        let identity = load_identity(data_dir, &snapshot.network.id)?;
        let device = identity.request().device_id_for(&snapshot.network.id)?;
        let effective = snapshot.effective_members(now)?;
        let member = effective
            .get(&device)
            .ok_or("prepared server is not a current member")?;
        if member.request != *identity.request()
            || member.kind != MemberKind::Server(identity.server_info().clone())
            || !member.permissions.is_full()
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
            },
            wireguard_listen_port: identity.server_info().wireguard_endpoint.port(),
        };
        config.validate()?;
        let directory = Directory::open(&config.network_dir(data_dir), true)?;
        for name in config.files.names().into_iter() {
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
