use std::path::Path;

use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::pkey::{Id, PKey, Private};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    DeviceId, DeviceKind, DeviceName, ENROLLMENT_NAMESPACE, ENROLLMENT_VERSION,
    EncryptionPublicKey, EnrollmentRequest, NetworkId, RequestId, Result, SshPublicKey,
    WgPublicKey, error::invalid,
};

pub struct LocalDeviceKeys {
    pub(crate) wg: PKey<Private>,
    pub(crate) encryption: PKey<Private>,
    ssh: ssh_key::PrivateKey,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretFile {
    version: u32,
    ssh_private_key: Zeroizing<String>,
    wg_private_key: Zeroizing<String>,
    encryption_private_key: Zeroizing<String>,
}

impl LocalDeviceKeys {
    pub fn generate() -> Result<Self> {
        Ok(Self {
            wg: PKey::generate_x25519()?,
            encryption: PKey::generate_x25519()?,
            ssh: ssh_key::PrivateKey::random(
                &mut ssh_key::rand_core::OsRng,
                ssh_key::Algorithm::Ed25519,
            )?,
        })
    }

    pub fn wg_public_key(&self) -> Result<WgPublicKey> {
        WgPublicKey::from_bytes(
            self.wg
                .raw_public_key()?
                .try_into()
                .map_err(|_| invalid("WG public key", "expected 32 bytes"))?,
        )
    }

    pub fn ssh_public_key(&self) -> Result<SshPublicKey> {
        let mut public = self.ssh.public_key().clone();
        public.set_comment("");
        public.to_openssh()?.parse()
    }

    pub fn encryption_public_key(&self) -> Result<EncryptionPublicKey> {
        EncryptionPublicKey::from_bytes(
            self.encryption
                .raw_public_key()?
                .try_into()
                .map_err(|_| invalid("encryption public key", "expected 32 bytes"))?,
        )
    }

    pub fn device_id(&self, network: &NetworkId, kind: DeviceKind) -> Result<DeviceId> {
        DeviceId::from_keys(
            network,
            kind,
            &self.ssh_public_key()?,
            &self.wg_public_key()?,
        )
    }

    /// Export only to the privileged WireGuard configurator, never a public profile.
    pub fn wireguard_private_key(&self) -> Result<Zeroizing<String>> {
        let raw = Zeroizing::new(self.wg.raw_private_key()?);
        Ok(Zeroizing::new(STANDARD.encode(raw.as_slice())))
    }

    /// Export only to a temporary owner-only identity file for the SSH client.
    /// Pass its path to SSH, never the key contents as an argument or log value.
    pub fn ssh_private_key(&self) -> Result<Zeroizing<String>> {
        Ok(self.ssh.to_openssh(ssh_key::LineEnding::LF)?)
    }

    pub fn sign(&self, namespace: &str, message: &[u8]) -> Result<ssh_key::SshSig> {
        Ok(self
            .ssh
            .sign(namespace, ssh_key::HashAlg::Sha256, message)?)
    }

    pub fn enrollment_request(
        &self,
        network_id: Option<NetworkId>,
        kind: DeviceKind,
        name: DeviceName,
    ) -> Result<EnrollmentRequest> {
        self.enrollment_request_expiring(network_id, kind, name, None)
    }

    pub fn enrollment_request_expiring(
        &self,
        network_id: Option<NetworkId>,
        kind: DeviceKind,
        name: DeviceName,
        expires_at: Option<u64>,
    ) -> Result<EnrollmentRequest> {
        let mut request = EnrollmentRequest {
            version: ENROLLMENT_VERSION,
            device_id: network_id
                .as_ref()
                .map(|network| self.device_id(network, kind))
                .transpose()?,
            network_id,
            kind,
            request_id: RequestId::random()?,
            name,
            wg_public_key: self.wg_public_key()?,
            ssh_public_key: self.ssh_public_key()?,
            encryption_public_key: self.encryption_public_key()?,
            expires_at,
            proof: String::new(),
        };
        request.proof = self
            .sign(ENROLLMENT_NAMESPACE, &request.canonical_bytes()?)?
            .to_pem(ssh_key::LineEnding::LF)?;
        Ok(request)
    }

    /// Atomically publishes a new 0600 key file without replacing any existing path.
    pub fn save_new(&self, path: &Path) -> Result<()> {
        let payload = SecretFile {
            version: 2,
            ssh_private_key: self.ssh_private_key()?,
            wg_private_key: self.wireguard_private_key()?,
            encryption_private_key: Zeroizing::new(
                STANDARD.encode(Zeroizing::new(self.encryption.raw_private_key()?).as_slice()),
            ),
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&payload)?);
        secure_file::save_new(path, &bytes)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = secure_file::load(path)?;
        let payload: SecretFile =
            serde_json::from_slice(&bytes).map_err(|_| invalid("key file", "invalid encoding"))?;
        if payload.version != 2 {
            return Err(invalid("key file", "unsupported version"));
        }
        let ssh = ssh_key::PrivateKey::from_openssh(payload.ssh_private_key.as_bytes())?;
        if ssh.algorithm() != ssh_key::Algorithm::Ed25519 || ssh.is_encrypted() {
            return Err(invalid(
                "SSH private key",
                "expected unencrypted Ed25519 key",
            ));
        }
        let raw = Zeroizing::new(
            STANDARD
                .decode(payload.wg_private_key.as_bytes())
                .map_err(|_| invalid("WG private key", "invalid base64"))?,
        );
        if raw.len() != 32
            || STANDARD.encode(raw.as_slice()) != *payload.wg_private_key
            || raw.iter().all(|b| *b == 0)
        {
            return Err(invalid("WG private key", "invalid 32-byte key"));
        }
        let encryption_raw = Zeroizing::new(
            STANDARD
                .decode(payload.encryption_private_key.as_bytes())
                .map_err(|_| invalid("encryption private key", "invalid base64"))?,
        );
        if encryption_raw.len() != 32
            || STANDARD.encode(encryption_raw.as_slice()) != *payload.encryption_private_key
            || encryption_raw.iter().all(|b| *b == 0)
            || encryption_raw.as_slice() == raw.as_slice()
        {
            return Err(invalid(
                "encryption private key",
                "expected a separate 32-byte key",
            ));
        }
        let keys = Self {
            ssh,
            wg: PKey::private_key_from_raw_bytes(&raw, Id::X25519)?,
            encryption: PKey::private_key_from_raw_bytes(&encryption_raw, Id::X25519)?,
        };
        keys.wg_public_key()?;
        keys.ssh_public_key()?;
        keys.encryption_public_key()?;
        Ok(keys)
    }
}

#[cfg(unix)]
mod secure_file {
    use crate::{Error, RequestId, Result, error::invalid};
    use rustix::{
        fd::OwnedFd,
        fs::{self, AtFlags, Mode, OFlags},
        process::geteuid,
    };
    use std::{
        ffi::OsString,
        fs::File,
        io::{Read, Write},
        os::unix::fs::MetadataExt,
        path::{Component, Path},
    };
    use zeroize::Zeroizing;

    fn parent(path: &Path) -> Result<(OwnedFd, OsString)> {
        let name = path
            .file_name()
            .ok_or_else(|| invalid("key path", "missing filename"))?
            .to_os_string();
        let mut dir = fs::open(
            if path.is_absolute() { "/" } else { "." },
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        for part in path.parent().unwrap_or_else(|| Path::new(".")).components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(part) => {
                    dir = fs::openat(
                        &dir,
                        part,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?
                }
                _ => return Err(invalid("key path", "parent traversal is forbidden")),
            }
        }
        let stat = fs::fstat(&dir)?;
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
            return Err(Error::UnsafeKeyFile);
        }
        Ok((dir, name))
    }

    pub(super) fn save_new(path: &Path, bytes: &[u8]) -> Result<()> {
        let (dir, name) = parent(path)?;
        let temp = format!(".tako-key-{}", RequestId::random()?);
        let fd = fs::openat(
            &dir,
            &temp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let result: Result<()> = (|| {
            let mut file = File::from(fd);
            fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            // linkat publishes without replacement; rename would clobber an existing key.
            fs::linkat(&dir, &temp, &dir, &name, AtFlags::empty())?;
            Ok(())
        })();
        let cleanup = fs::unlinkat(&dir, &temp, AtFlags::empty());
        result?;
        cleanup?;
        fs::fsync(&dir)?;
        Ok(())
    }

    pub(super) fn load(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
        let (dir, name) = parent(path)?;
        let fd = fs::openat(
            &dir,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let file = File::from(fd);
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != geteuid().as_raw()
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(Error::UnsafeKeyFile);
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 {
            return Err(invalid("key file", "too large"));
        }
        Ok(bytes)
    }
}

#[cfg(not(unix))]
mod secure_file {
    use crate::{Error, Result};
    use std::path::Path;
    use zeroize::Zeroizing;
    pub(super) fn save_new(_: &Path, _: &[u8]) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn load(_: &Path) -> Result<Zeroizing<Vec<u8>>> {
        Err(Error::UnsupportedPlatform)
    }
}
