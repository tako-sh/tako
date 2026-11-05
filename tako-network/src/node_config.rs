//! Public bootstrap configuration for the separately supervised native agent.
//! Filenames are relative to `data_dir/networks/<network-id>`. No secrets belong
//! in this document; prepared credentials live in protected, explicit files.
use crate::{DeviceId, MemberKind, NetworkDescriptor, RegistrySnapshot, Result, error::invalid};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub const NODE_CONFIG_FILE: &str = "native-network.json";
pub const MAX_NODE_CONFIG_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub version: u32,
    pub network: NetworkDescriptor,
    pub server_id: DeviceId,
    pub bootstrap: RegistrySnapshot,
    pub files: NodeFiles,
    /// SHA-256 of the installed executable, not the compressed release archive.
    pub corrosion_binary_sha256: String,
    pub wireguard_listen_port: u16,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeFiles {
    pub keys: String,
    pub https_certificate: String,
    pub https_private_key: String,
    pub gossip_ca_certificate: String,
    pub gossip_certificate: String,
    pub gossip_private_key: String,
    pub api_token: String,
}

impl NodeFiles {
    pub fn names(&self) -> [&str; 7] {
        [
            &self.keys,
            &self.https_certificate,
            &self.https_private_key,
            &self.gossip_ca_certificate,
            &self.gossip_certificate,
            &self.gossip_private_key,
            &self.api_token,
        ]
    }
}

impl NodeConfig {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 || self.network != self.bootstrap.network {
            return Err(invalid("node config", "version or network mismatch"));
        }
        self.bootstrap.validate()?;
        if self.bootstrap.members.len()
            + self.bootstrap.revocations.len()
            + self.bootstrap.acks.len()
            >= crate::corrosion::MAX_REGISTRY_ROWS
            || serde_json::to_vec(self)?.len() > MAX_NODE_CONFIG_BYTES
        {
            return Err(invalid("node config", "snapshot exceeds bounds"));
        }
        if !self.bootstrap.members.iter().any(|member| {
            member.device_id == self.server_id
                && member.wg_verified
                && matches!(member.kind, MemberKind::Server(_))
                && member.permissions.server_admin
        }) {
            return Err(invalid("node config", "missing admitted server"));
        }
        if self.wireguard_listen_port < 1024 || self.wireguard_listen_port == 8787 {
            return Err(invalid("node config", "invalid WireGuard port"));
        }
        if self.corrosion_binary_sha256.len() != 64
            || !self
                .corrosion_binary_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(invalid(
                "node config",
                "expected lowercase executable SHA-256",
            ));
        }
        let mut names = BTreeSet::new();
        for name in self.files.names() {
            validate_filename(name)?;
            if !names.insert(name)
                || RESERVED.contains(&name)
                || name.starts_with("agent-")
                || name.starts_with("corrosion.")
                || name.starts_with("runtime.")
            {
                return Err(invalid(
                    "node filename",
                    "duplicate or agent-owned filename",
                ));
            }
        }
        Ok(())
    }

    pub fn network_dir(&self, data_dir: &Path) -> PathBuf {
        data_dir.join("networks").join(self.network.id.to_string())
    }
    pub fn socket_path(&self, data_dir: &Path) -> PathBuf {
        self.network_dir(data_dir).join("agent.sock")
    }
    #[cfg(unix)]
    pub fn load(data_dir: &Path) -> Result<Self> {
        let directory = protected::Directory::open(data_dir, false)?;
        let bytes = directory.read(NODE_CONFIG_FILE, MAX_NODE_CONFIG_BYTES)?;
        let config: Self = serde_json::from_slice(&bytes)?;
        config.validate()?;
        Ok(config)
    }
    #[cfg(not(unix))]
    pub fn load(_: &Path) -> Result<Self> {
        Err(crate::Error::UnsupportedPlatform)
    }
}

const RESERVED: &[&str] = &[
    "local-services.json",
    "services.json",
    "service-publication.json",
    "agent.sock",
    "desired.json",
    "known-registry.json",
    "bootstrap-complete",
    "corrosion.toml",
    "corrosion.db",
    "corrosion.db-wal",
    "corrosion.db-shm",
    "corrosion.schema.sql",
    "corrosion-ca.pem",
    "corrosion-admin.sock",
    "subscriptions",
    "runtime.db",
    "runtime.db-wal",
    "runtime.db-shm",
    "runtime.runtime-lock",
];

pub fn validate_filename(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(invalid(
            "node filename",
            "expected one safe relative filename",
        ));
    }
    Ok(())
}

/// Descriptor-relative no-follow access shared with the agent. No secret Debug.
#[cfg(unix)]
pub mod protected {
    use super::validate_filename;
    use crate::{Error, RequestId, Result, error::invalid};
    use rustix::{
        fs::{self, AtFlags, Mode, OFlags},
        process::geteuid,
    };
    use std::{
        fs::File,
        io::{Read, Write},
        os::unix::fs::MetadataExt,
        path::{Component, Path},
    };
    use zeroize::Zeroizing;

    pub struct Directory(File);
    impl Directory {
        pub fn open(path: &Path, private: bool) -> Result<Self> {
            if !path.is_absolute() {
                return Err(invalid("node directory", "absolute path required"));
            }
            let mut fd = fs::open(
                "/",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            for component in path.components() {
                match component {
                    Component::RootDir => (),
                    Component::Normal(name) => {
                        fd = fs::openat(
                            &fd,
                            name,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )?;
                        let stat = fs::fstat(&fd)?;
                        // A sticky root-owned /tmp is safe for private test directories.
                        if (stat.st_uid != 0 && stat.st_uid != geteuid().as_raw())
                            || (stat.st_mode & 0o022 != 0
                                && !(stat.st_uid == 0 && stat.st_mode & 0o1000 != 0))
                        {
                            return Err(Error::UnsafeKeyFile);
                        }
                    }
                    _ => return Err(invalid("node directory", "path traversal forbidden")),
                }
            }
            let file = File::from(fd);
            let meta = file.metadata()?;
            if private && (meta.uid() != geteuid().as_raw() || meta.mode() & 0o7777 != 0o700) {
                return Err(Error::UnsafeKeyFile);
            }
            Ok(Self(file))
        }
        /// Atomically open/create a lock without replacing its inode. All
        /// contenders must lock this same file; atomic rename is unsafe here.
        pub fn lock(&self, name: &str) -> Result<File> {
            validate_filename(name)?;
            let fd = fs::openat(
                &self.0,
                name,
                OFlags::RDWR
                    | OFlags::CREATE
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?;
            let file = File::from(fd);
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.nlink() != 1
                || meta.uid() != geteuid().as_raw()
                || meta.mode() & 0o7777 != 0o600
            {
                return Err(Error::UnsafeKeyFile);
            }
            file.try_lock()
                .map_err(|_| invalid("agent lock", "already held by another process"))?;
            Ok(file)
        }
        pub fn open_file(&self, name: &str) -> Result<File> {
            validate_filename(name)?;
            let fd = fs::openat(
                &self.0,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let file = File::from(fd);
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.nlink() != 1
                || meta.uid() != geteuid().as_raw()
                || meta.mode() & 0o7777 != 0o600
            {
                return Err(Error::UnsafeKeyFile);
            }
            Ok(file)
        }
        pub fn read(&self, name: &str, limit: usize) -> Result<Zeroizing<Vec<u8>>> {
            let file = self.open_file(name)?;
            let mut bytes = Zeroizing::new(Vec::new());
            file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > limit {
                return Err(invalid("protected file", "size limit exceeded"));
            }
            Ok(bytes)
        }
        pub fn exists(&self, name: &str) -> Result<bool> {
            match self.open_file(name) {
                Ok(_) => Ok(true),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        }
        pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
            validate_filename(name)?;
            // Refuse to replace an unexpected symlink, hardlink, or non-private file.
            self.exists(name)?;
            let temporary = format!("agent-tmp-{}", RequestId::random()?);
            let result = (|| {
                let fd = fs::openat(
                    &self.0,
                    &temporary,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )?;
                let mut file = File::from(fd);
                fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                fs::renameat(&self.0, &temporary, &self.0, name)?;
                fs::fsync(&self.0)?;
                Ok(())
            })();
            let _ = fs::unlinkat(&self.0, &temporary, AtFlags::empty());
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filenames_are_single_components() {
        for name in [
            "../key", "/key", "a/key", "a\\key", ".", "..", "", "key\n", ".hidden",
        ] {
            assert!(validate_filename(name).is_err(), "{name:?}");
        }
        validate_filename("gossip-leaf.pem").unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn protected_files_reject_links_permissions_and_oversize() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let dir = protected::Directory::open(temp.path(), true).unwrap();
        dir.write("key", b"private").unwrap();
        assert_eq!(&**dir.read("key", 7).unwrap(), b"private");
        assert!(dir.read("key", 6).is_err());
        symlink("key", temp.path().join("link")).unwrap();
        assert!(dir.read("link", 100).is_err());
        assert!(dir.write("link", b"bad").is_err());
        std::fs::hard_link(temp.path().join("key"), temp.path().join("hard")).unwrap();
        assert!(dir.read("key", 100).is_err());
        std::fs::remove_file(temp.path().join("hard")).unwrap();
        std::fs::set_permissions(
            temp.path().join("key"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(dir.read("key", 100).is_err());
        assert!(protected::Directory::open(&temp.path().join(".."), false).is_err());
    }
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::protected::Directory;
    #[test]
    fn concurrent_agent_lock_never_replaces_the_inode() {
        let temp = tempfile::tempdir().unwrap();
        let dir = Directory::open(temp.path(), true).unwrap();
        let held = dir.lock("agent-process-lock").unwrap();
        assert!(dir.lock("agent-process-lock").is_err());
        drop(held);
        dir.lock("agent-process-lock").unwrap();
    }
}
