//! Local-only server preparation. Public HTTPS pins identify a per-server CA
//! issuer, not a leaf: clients verify its chain and the endpoint DNS name/IP.
//! Admission, public snapshot publication, TLS renewal and listeners belong to
//! the caller. CA private keys must never be included in enrollment or profiles.

use std::{
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    stack::Stack,
    x509::{
        X509, X509Builder, X509NameBuilder, X509PurposeId, X509StoreContext,
        extension::{
            AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage,
            SubjectAlternativeName, SubjectKeyIdentifier,
        },
        store::X509StoreBuilder,
        verify::{X509VerifyFlags, X509VerifyParam},
    },
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    DeviceKind, DeviceName, EnrollmentRequest, LocalDeviceKeys, NetworkId, Result, ServerInfo,
    SshPublicKey, WgChallenge, WgProof, error::invalid,
};

const METADATA: &str = "identity-prepared.json";
const DEVICE_KEYS: &str = "device-keys.json";
const MAX_FILE_BYTES: usize = 64 * 1024;

/// Host-local paths. Only `ca_certificate` is a public trust issuer; the leaf
/// and fullchain are served by TLS. Retain the CA key locally for leaf renewal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerTlsPaths {
    pub ca_certificate: PathBuf,
    pub ca_private_key: PathBuf,
    pub certificate: PathBuf,
    pub fullchain: PathBuf,
    pub private_key: PathBuf,
}

impl ServerTlsPaths {
    fn new(directory: &Path, prefix: &str) -> Self {
        Self {
            ca_certificate: directory.join(format!("{prefix}-ca.pem")),
            ca_private_key: directory.join(format!("{prefix}-ca-key.pem")),
            certificate: directory.join(format!("{prefix}-leaf.pem")),
            fullchain: directory.join(format!("{prefix}-fullchain.pem")),
            private_key: directory.join(format!("{prefix}-key.pem")),
        }
    }
}

pub struct ServerIdentity {
    keys: LocalDeviceKeys,
    keys_path: PathBuf,
    prepared: Prepared,
    tls: ServerTlsPaths,
    gossip: ServerTlsPaths,
}

impl fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("network_id", &self.prepared.config.network_id)
            .field("device_id", &self.prepared.request.device_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    network_id: NetworkId,
    name: DeviceName,
    wireguard_endpoint: SocketAddr,
    ssh_host_public_key: SshPublicKey,
    ssh_port: u16,
    https_port: u16,
}

impl Config {
    fn server_info(&self) -> ServerInfo {
        ServerInfo {
            wireguard_endpoint: self.wireguard_endpoint,
            ssh_host_key: self.ssh_host_public_key.clone(),
            ssh_port: self.ssh_port,
            https_port: self.https_port,
            https_certificate_pem: None,
            https_spki_sha256: None,
            corrosion_ca_certificate_pem: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    version: u32,
    config: Config,
    request: EnrollmentRequest,
    server_info: ServerInfo,
}

impl Prepared {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let prepared: Self = serde_json::from_slice(bytes)
            .map_err(|_| invalid("server identity", "invalid prepared metadata"))?;
        if prepared.version != 1 {
            return Err(invalid("server identity", "unsupported prepared version"));
        }
        Ok(prepared)
    }
}

impl ServerIdentity {
    /// Load and validate an already prepared identity without creating or
    /// changing any files. Missing, partial, unsafe or corrupt state is an error.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = std::path::absolute(directory)?;
        let dir = secure::Directory::open(&directory, false)?;
        let bytes = dir
            .read_optional(METADATA)?
            .ok_or_else(|| invalid("server identity", "identity is not prepared"))?;
        Self::load_prepared(&directory, &dir, Prepared::parse(&bytes)?)
    }

    fn load_prepared(
        directory: &Path,
        dir: &secure::Directory,
        prepared: Prepared,
    ) -> Result<Self> {
        let keys_path = directory.join(DEVICE_KEYS);
        let identity = Self {
            keys: LocalDeviceKeys::load(&keys_path)?,
            keys_path,
            prepared,
            tls: ServerTlsPaths::new(directory, "https"),
            gossip: ServerTlsPaths::new(directory, "gossip"),
        };
        identity.validate(dir)?;
        dir.check_path(directory)?;
        Ok(identity)
    }

    /// Create or load a persistent, network-bound server identity. The parent
    /// directory must exist. The final directory must be owned mode 0700 and
    /// empty on first use; no path component may be a symlink or `..`.
    ///
    /// A partial prepare, changed configuration, unsafe file, or corrupt state
    /// fails closed. Nothing is silently repaired, replaced or rotated.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        directory: impl AsRef<Path>,
        network_id: NetworkId,
        name: DeviceName,
        wireguard_endpoint: SocketAddr,
        ssh_host_public_key: SshPublicKey,
        ssh_port: u16,
        https_port: u16,
    ) -> Result<Self> {
        let config = Config {
            network_id,
            name,
            wireguard_endpoint,
            ssh_host_public_key,
            ssh_port,
            https_port,
        };
        config.server_info().validate()?;
        let directory = std::path::absolute(directory)?;
        let dir = secure::Directory::open(&directory, true)?;
        if let Some(bytes) = dir.read_optional(METADATA)? {
            let prepared = Prepared::parse(&bytes)?;
            if prepared.config != config {
                return Err(invalid(
                    "server identity",
                    "network or configuration mismatch",
                ));
            }
            return Self::load_prepared(&directory, &dir, prepared);
        }
        if !dir.is_empty()? {
            return Err(invalid(
                "server identity",
                "partial preparation or nonempty directory",
            ));
        }
        // Durable, exclusive reservation makes concurrent/partial preparation fail closed.
        dir.write_new("prepare-started", b"1\n")?;
        let keys = LocalDeviceKeys::generate()?;
        let keys_path = directory.join(DEVICE_KEYS);
        keys.save_new(&keys_path)?;
        dir.check_path(&directory)?;
        let request = keys.enrollment_request(
            Some(config.network_id.clone()),
            DeviceKind::Server,
            config.name.clone(),
        )?;
        let names = certificate_names(&config, &request)?;
        let tls = ServerTlsPaths::new(&directory, "https");
        let gossip = ServerTlsPaths::new(&directory, "gossip");
        let https_ca = generate_tls(&dir, &tls, &names, false)?;
        let gossip_ca = generate_tls(&dir, &gossip, &names, true)?;
        let mut server_info = config.server_info();
        server_info.https_certificate_pem = Some(https_ca);
        server_info.corrosion_ca_certificate_pem = Some(gossip_ca);
        server_info.https_spki_sha256 = server_info.https_spki_fingerprint()?;
        let identity = Self {
            keys,
            keys_path,
            prepared: Prepared {
                version: 1,
                config,
                request,
                server_info,
            },
            tls,
            gossip,
        };
        identity.validate(&dir)?;
        let metadata = serde_json::to_vec(&identity.prepared)?;
        dir.write_new(METADATA, &metadata)?;
        dir.check_path(&directory)?;
        Ok(identity)
    }

    pub fn keys(&self) -> &LocalDeviceKeys {
        &self.keys
    }

    /// Absolute local device-key path. Use its `file_name()` for a single-component
    /// NodeFiles entry relative to the identity directory.
    pub fn keys_path(&self) -> &Path {
        &self.keys_path
    }

    pub fn request(&self) -> &EnrollmentRequest {
        &self.prepared.request
    }

    /// HTTPS certificate/SPKI and Corrosion certificate pin the independent
    /// per-server CA issuers. These fields never contain private keys or leaves.
    pub fn server_info(&self) -> &ServerInfo {
        &self.prepared.server_info
    }
    pub fn tls_paths(&self) -> &ServerTlsPaths {
        &self.tls
    }
    pub fn gossip_paths(&self) -> &ServerTlsPaths {
        &self.gossip
    }

    /// The caller must authenticate the bootstrap channel before answering.
    pub fn answer_challenge(&self, challenge: &WgChallenge, now: u64) -> Result<WgProof> {
        self.keys.answer_challenge(challenge, self.request(), now)
    }

    fn validate(&self, dir: &secure::Directory) -> Result<()> {
        let config = &self.prepared.config;
        let request = self.request();
        request.verify()?;
        if request.network_id.as_ref() != Some(&config.network_id)
            || request.kind != DeviceKind::Server
            || request.name != config.name
            || request.expires_at.is_some()
            || request.wg_public_key != self.keys.wg_public_key()?
            || request.ssh_public_key != self.keys.ssh_public_key()?
        {
            return Err(invalid(
                "server identity",
                "request does not bind stored server keys",
            ));
        }
        let names = certificate_names(config, request)?;
        let mut expected = config.server_info();
        expected.https_certificate_pem = Some(validate_tls(dir, &self.tls, &names, false)?);
        expected.corrosion_ca_certificate_pem =
            Some(validate_tls(dir, &self.gossip, &names, true)?);
        expected.https_spki_sha256 = expected.https_spki_fingerprint()?;
        expected.validate()?;
        if expected != self.prepared.server_info {
            return Err(invalid(
                "server identity",
                "public pins or endpoint mismatch",
            ));
        }
        if expected.https_certificate_pem == expected.corrosion_ca_certificate_pem {
            return Err(invalid(
                "server identity",
                "HTTPS and gossip issuers must be independent",
            ));
        }
        Ok(())
    }
}

struct CertificateNames {
    hostname: String,
    gossip: String,
    ips: [std::net::IpAddr; 2],
}

fn certificate_names(config: &Config, request: &EnrollmentRequest) -> Result<CertificateNames> {
    let id = request.device_id_for(&config.network_id)?.to_string();
    Ok(CertificateNames {
        hostname: format!("node-{}.{}.tako.internal", &id[..32], config.network_id),
        gossip: format!("{}.corrosion.tako.internal", config.network_id),
        ips: [
            config.wireguard_endpoint.ip().to_canonical(),
            config.network_id.address(&request.wg_public_key).into(),
        ],
    })
}

fn new_key() -> Result<PKey<Private>> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    Ok(PKey::from_ec_key(EcKey::generate(&group)?)?)
}

fn certificate(key: &PKey<Private>, common_name: &str, days: u32) -> Result<X509Builder> {
    let mut builder = X509::builder()?;
    builder.set_version(2)?;
    let mut serial = BigNum::new()?;
    serial.rand(159, MsbOption::ONE, false)?;
    let serial = serial.to_asn1_integer()?;
    builder.set_serial_number(&serial)?;
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, common_name)?;
    let name = name.build();
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(&name)?;
    builder.set_pubkey(key)?;
    let not_before = Asn1Time::days_from_now(0)?;
    let not_after = Asn1Time::days_from_now(days)?;
    builder.set_not_before(&not_before)?;
    builder.set_not_after(&not_after)?;
    Ok(builder)
}

fn generate_tls(
    dir: &secure::Directory,
    paths: &ServerTlsPaths,
    names: &CertificateNames,
    gossip: bool,
) -> Result<String> {
    let ca_key = new_key()?;
    let leaf_key = new_key()?;
    let mut ca = certificate(
        &ca_key,
        if gossip {
            "Tako gossip CA"
        } else {
            "Tako HTTPS CA"
        },
        3650,
    )?;
    ca.append_extension(BasicConstraints::new().critical().ca().pathlen(0).build()?)?;
    ca.append_extension(
        KeyUsage::new()
            .critical()
            .key_cert_sign()
            .crl_sign()
            .build()?,
    )?;
    let ski = SubjectKeyIdentifier::new().build(&ca.x509v3_context(None, None))?;
    ca.append_extension(ski)?;
    ca.sign(&ca_key, MessageDigest::sha256())?;
    let ca = ca.build();
    let mut leaf = certificate(
        &leaf_key,
        if gossip { "Tako gossip" } else { "Tako HTTPS" },
        365,
    )?;
    leaf.set_issuer_name(ca.subject_name())?;
    leaf.append_extension(BasicConstraints::new().critical().build()?)?;
    leaf.append_extension(KeyUsage::new().critical().digital_signature().build()?)?;
    let mut eku = ExtendedKeyUsage::new();
    eku.server_auth();
    if gossip {
        eku.client_auth();
    }
    leaf.append_extension(eku.build()?)?;
    let mut san = SubjectAlternativeName::new();
    san.dns(&names.hostname);
    if gossip {
        san.dns(&names.gossip);
    }
    for ip in names.ips {
        san.ip(&ip.to_string());
    }
    let san = san.build(&leaf.x509v3_context(Some(&ca), None))?;
    leaf.append_extension(san)?;
    let aki = AuthorityKeyIdentifier::new()
        .keyid(true)
        .build(&leaf.x509v3_context(Some(&ca), None))?;
    leaf.append_extension(aki)?;
    let ski = SubjectKeyIdentifier::new().build(&leaf.x509v3_context(Some(&ca), None))?;
    leaf.append_extension(ski)?;
    leaf.sign(&ca_key, MessageDigest::sha256())?;
    let ca_pem = ca.to_pem()?;
    let leaf_pem = leaf.build().to_pem()?;
    let ca_secret = Zeroizing::new(ca_key.private_key_to_pem_pkcs8()?);
    let leaf_secret = Zeroizing::new(leaf_key.private_key_to_pem_pkcs8()?);
    dir.write_path(&paths.ca_private_key, &ca_secret)?;
    dir.write_path(&paths.private_key, &leaf_secret)?;
    dir.write_path(&paths.ca_certificate, &ca_pem)?;
    dir.write_path(&paths.certificate, &leaf_pem)?;
    dir.write_path(&paths.fullchain, &[leaf_pem, ca_pem.clone()].concat())?;
    pem_string(ca_pem)
}

fn pem_string(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| invalid("certificate", "invalid PEM encoding"))
}

fn validate_tls(
    dir: &secure::Directory,
    paths: &ServerTlsPaths,
    names: &CertificateNames,
    gossip: bool,
) -> Result<String> {
    let ca_bytes = dir.read_path(&paths.ca_certificate)?;
    let leaf_bytes = dir.read_path(&paths.certificate)?;
    let ca = X509::from_pem(&ca_bytes)?;
    let leaf = X509::from_pem(&leaf_bytes)?;
    if ca.to_pem()? != *ca_bytes
        || leaf.to_pem()? != *leaf_bytes
        || *dir.read_path(&paths.fullchain)?
            != [leaf_bytes.as_slice(), ca_bytes.as_slice()].concat()
    {
        return Err(invalid(
            "server TLS",
            "expected canonical certificate and fullchain",
        ));
    }
    let ca_key = PKey::private_key_from_pem(&dir.read_path(&paths.ca_private_key)?)?;
    let leaf_key = PKey::private_key_from_pem(&dir.read_path(&paths.private_key)?)?;
    if !ca.public_key()?.public_eq(&ca_key)
        || !leaf.public_key()?.public_eq(&leaf_key)
        || ca_key.public_eq(&leaf_key)
        || !ca.verify(&ca_key)?
        || ca.subject_name().to_der()? != ca.issuer_name().to_der()?
    {
        return Err(invalid("server TLS", "certificate/key or issuer mismatch"));
    }
    let sans = leaf
        .subject_alt_names()
        .ok_or_else(|| invalid("server TLS", "missing SANs"))?;
    let dns: Vec<_> = sans.iter().filter_map(|san| san.dnsname()).collect();
    let expected_dns = if gossip {
        vec![names.hostname.as_str(), names.gossip.as_str()]
    } else {
        vec![names.hostname.as_str()]
    };
    let ips: Vec<_> = sans.iter().filter_map(|san| san.ipaddress()).collect();
    let expected_ips: Vec<Vec<u8>> = names
        .ips
        .iter()
        .map(|ip| match ip {
            std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
            std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
        })
        .collect();
    if dns != expected_dns || ips != expected_ips || sans.len() != dns.len() + ips.len() {
        return Err(invalid(
            "server TLS",
            "SANs do not match server configuration",
        ));
    }
    verify_leaf(&ca, &leaf, X509PurposeId::SSL_SERVER, &names.hostname)?;
    if gossip {
        verify_leaf(&ca, &leaf, X509PurposeId::SSL_CLIENT, &names.gossip)?;
    }
    pem_string(ca_bytes.to_vec())
}

fn verify_leaf(ca: &X509, leaf: &X509, purpose: X509PurposeId, hostname: &str) -> Result<()> {
    let mut store = X509StoreBuilder::new()?;
    store.add_cert(ca.clone())?;
    let mut param = X509VerifyParam::new()?;
    param.set_flags(X509VerifyFlags::X509_STRICT)?;
    param.set_purpose(purpose)?;
    param.set_host(hostname)?;
    store.set_param(&param)?;
    let chain = Stack::new()?;
    let valid = X509StoreContext::new()?.init(&store.build(), leaf, &chain, |context| {
        context.verify_cert()
    })?;
    if !valid {
        return Err(invalid(
            "server TLS",
            "chain, validity, name or purpose mismatch",
        ));
    }
    Ok(())
}

#[cfg(unix)]
mod secure {
    use super::*;
    use crate::Error;
    use rustix::{
        fd::OwnedFd,
        fs::{self, Mode, OFlags},
        process::geteuid,
    };
    use std::{
        fs::File,
        io::{Read, Write},
        os::unix::fs::MetadataExt,
        path::Component,
    };

    pub(super) struct Directory(OwnedFd);

    impl Directory {
        pub(super) fn open(path: &Path, create: bool) -> Result<Self> {
            let mut fd = fs::open(
                "/",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let components: Vec<_> = path.components().collect();
            if !path.is_absolute() || path.file_name().is_none() {
                return Err(invalid(
                    "identity directory",
                    "expected a named absolute directory",
                ));
            }
            for (index, part) in components.iter().enumerate() {
                match part {
                    Component::RootDir | Component::CurDir => {}
                    Component::Normal(name) => {
                        if create && index == components.len() - 1 {
                            match fs::mkdirat(&fd, *name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                                Ok(()) => fs::fsync(&fd)?,
                                Err(rustix::io::Errno::EXIST) => {}
                                Err(error) => return Err(error.into()),
                            }
                        }
                        fd = fs::openat(
                            &fd,
                            *name,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )?;
                    }
                    _ => {
                        return Err(invalid(
                            "identity directory",
                            "parent traversal is forbidden",
                        ));
                    }
                }
            }
            let stat = fs::fstat(&fd)?;
            if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o7777 != 0o700 {
                return Err(Error::UnsafeKeyFile);
            }
            Ok(Self(fd))
        }

        pub(super) fn check_path(&self, path: &Path) -> Result<()> {
            let current = Self::open(path, false)?;
            let before = fs::fstat(&self.0)?;
            let after = fs::fstat(&current.0)?;
            if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
                return Err(invalid(
                    "identity directory",
                    "directory changed during preparation",
                ));
            }
            Ok(())
        }

        pub(super) fn is_empty(&self) -> Result<bool> {
            for entry in fs::Dir::read_from(&self.0)? {
                let entry = entry?;
                if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
                    return Ok(false);
                }
            }
            Ok(true)
        }

        pub(super) fn read_optional(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
            let fd = match fs::openat(
                &self.0,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let file = File::from(fd);
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.uid() != geteuid().as_raw()
                || meta.mode() & 0o7777 != 0o600
                || meta.nlink() != 1
            {
                return Err(Error::UnsafeKeyFile);
            }
            let mut bytes = Zeroizing::new(Vec::new());
            file.take(MAX_FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_FILE_BYTES {
                return Err(invalid("identity file", "too large"));
            }
            Ok(Some(bytes))
        }

        pub(super) fn read_path(&self, path: &Path) -> Result<Zeroizing<Vec<u8>>> {
            self.read_optional(filename(path)?)?
                .ok_or_else(|| invalid("server identity", "partial preparation: missing file"))
        }

        pub(super) fn write_path(&self, path: &Path, bytes: &[u8]) -> Result<()> {
            self.write_new(filename(path)?, bytes)
        }

        pub(super) fn write_new(&self, name: &str, bytes: &[u8]) -> Result<()> {
            if bytes.len() > MAX_FILE_BYTES {
                return Err(invalid("identity file", "too large"));
            }
            let fd = fs::openat(
                &self.0,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?;
            let mut file = File::from(fd);
            fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::fsync(&self.0)?;
            Ok(())
        }
    }

    fn filename(path: &Path) -> Result<&str> {
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("identity path", "invalid filename"))
    }
}

#[cfg(not(unix))]
mod secure {
    use super::*;
    pub(super) struct Directory;
    impl Directory {
        pub(super) fn open(_: &Path, _: bool) -> Result<Self> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn check_path(&self, _: &Path) -> Result<()> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn is_empty(&self) -> Result<bool> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn read_optional(&self, _: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn read_path(&self, _: &Path) -> Result<Zeroizing<Vec<u8>>> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn write_path(&self, _: &Path, _: &[u8]) -> Result<()> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn write_new(&self, _: &str, _: &[u8]) -> Result<()> {
            Err(crate::Error::UnsupportedPlatform)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn prepare(path: &Path) -> Result<ServerIdentity> {
        ServerIdentity::prepare(
            path,
            "1234567890abcdef1234567890abcdef".parse()?,
            "test-server".parse()?,
            "192.0.2.42:51820".parse().unwrap(),
            host_key(),
            22,
            443,
        )
    }

    fn host_key() -> SshPublicKey {
        use std::sync::OnceLock;
        static KEY: OnceLock<SshPublicKey> = OnceLock::new();
        KEY.get_or_init(|| {
            LocalDeviceKeys::generate()
                .unwrap()
                .ssh_public_key()
                .unwrap()
        })
        .clone()
    }

    #[test]
    fn load_preserves_identity_and_exposes_the_local_keys_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("identity");
        let first = prepare(&path).unwrap();
        let before = snapshot(&path);
        let loaded = ServerIdentity::load(&path).unwrap();
        assert_eq!(loaded.request(), first.request());
        assert_eq!(loaded.server_info(), first.server_info());
        assert_eq!(loaded.tls_paths(), first.tls_paths());
        assert_eq!(loaded.gossip_paths(), first.gossip_paths());
        assert_eq!(loaded.keys_path(), first.keys_path());
        assert_eq!(loaded.keys_path(), path.join(DEVICE_KEYS));
        let filename = Path::new(loaded.keys_path().file_name().unwrap());
        assert_eq!(filename.components().count(), 1);
        let keys = LocalDeviceKeys::load(&path.join(filename)).unwrap();
        assert_eq!(
            keys.wg_public_key().unwrap(),
            loaded.request().wg_public_key
        );
        assert_eq!(
            keys.ssh_public_key().unwrap(),
            loaded.request().ssh_public_key
        );
        let (challenge, pending) = WgChallenge::begin(
            first.request(),
            first.prepared.config.network_id.clone(),
            100,
            200,
        )
        .unwrap();
        pending
            .verify(&loaded.answer_challenge(&challenge, 101).unwrap(), 101)
            .unwrap();
        assert_eq!(snapshot(&path), before);
    }

    #[test]
    fn load_never_creates_or_completes_unprepared_state() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        assert!(ServerIdentity::load(&missing).is_err());
        assert!(!missing.exists());
        assert!(ServerIdentity::load(temp.path()).is_err());
        assert!(snapshot(temp.path()).is_empty());
        prepare(temp.path()).unwrap();
        std::fs::remove_file(temp.path().join(METADATA)).unwrap();
        let before = snapshot(temp.path());
        assert!(
            ServerIdentity::load(temp.path())
                .unwrap_err()
                .to_string()
                .contains("not prepared")
        );
        assert_eq!(snapshot(temp.path()), before);
    }

    #[test]
    fn restart_preserves_keys_request_and_public_pins() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("identity");
        let first = prepare(&path).unwrap();
        let second = prepare(&path).unwrap();
        assert_eq!(first.request(), second.request());
        assert_eq!(first.server_info(), second.server_info());
        assert_eq!(
            first.keys().wireguard_private_key().unwrap(),
            second.keys().wireguard_private_key().unwrap()
        );
        assert_eq!(
            first.keys().ssh_private_key().unwrap(),
            second.keys().ssh_private_key().unwrap()
        );
        assert_eq!(first.tls_paths(), second.tls_paths());
        let (challenge, pending) = WgChallenge::begin(
            first.request(),
            first.prepared.config.network_id.clone(),
            100,
            200,
        )
        .unwrap();
        let verified = pending
            .verify(&second.answer_challenge(&challenge, 101).unwrap(), 101)
            .unwrap();
        assert_eq!(verified.request().kind, DeviceKind::Server);
        assert_eq!(verified.request(), first.request());
        assert!(!format!("{first:?}").contains("PRIVATE"));
    }
    fn openssl_verify(paths: &ServerTlsPaths, purpose: &str, name_flag: &str, name: &str) -> bool {
        let result = std::process::Command::new("openssl")
            .args(["verify", "-x509_strict", "-CAfile"])
            .arg(&paths.ca_certificate)
            .args(["-purpose", purpose, name_flag, name])
            .arg(&paths.certificate)
            .output()
            .unwrap();
        result.status.success()
    }

    #[test]
    fn openssl_verifies_chain_sans_and_distinct_tls_roles() {
        let temp = tempfile::tempdir().unwrap();
        let identity = prepare(temp.path()).unwrap();
        let names = certificate_names(&identity.prepared.config, identity.request()).unwrap();
        let tls = identity.tls_paths();
        let gossip = identity.gossip_paths();
        assert!(openssl_verify(
            tls,
            "sslserver",
            "-verify_hostname",
            &names.hostname
        ));
        assert!(!openssl_verify(
            tls,
            "sslclient",
            "-verify_hostname",
            &names.hostname
        ));
        assert!(!openssl_verify(
            tls,
            "sslserver",
            "-verify_hostname",
            &names.gossip
        ));
        assert!(!openssl_verify(
            tls,
            "sslserver",
            "-verify_hostname",
            "wrong.tako.internal"
        ));
        assert!(!openssl_verify(
            tls,
            "sslserver",
            "-verify_ip",
            "192.0.2.99"
        ));
        for ip in names.ips {
            assert!(openssl_verify(
                tls,
                "sslserver",
                "-verify_ip",
                &ip.to_string()
            ));
            assert!(openssl_verify(
                gossip,
                "sslserver",
                "-verify_ip",
                &ip.to_string()
            ));
        }
        for hostname in [&names.hostname, &names.gossip] {
            for purpose in ["sslclient", "sslserver"] {
                assert!(openssl_verify(
                    gossip,
                    purpose,
                    "-verify_hostname",
                    hostname
                ));
            }
        }
        let mut wrong_issuer = tls.clone();
        wrong_issuer.ca_certificate = gossip.ca_certificate.clone();
        assert!(!openssl_verify(
            &wrong_issuer,
            "sslserver",
            "-verify_hostname",
            &names.hostname
        ));
    }

    #[test]
    fn all_tls_keys_are_independent_and_match_their_certificates() {
        use std::{collections::BTreeSet, fs};
        let temp = tempfile::tempdir().unwrap();
        let first = prepare(&temp.path().join("first")).unwrap();
        let second = prepare(&temp.path().join("second")).unwrap();
        let mut public_keys = BTreeSet::new();
        for identity in [&first, &second] {
            let info = identity.server_info();
            for (paths, pin) in [
                (
                    identity.tls_paths(),
                    info.https_certificate_pem.as_ref().unwrap(),
                ),
                (
                    identity.gossip_paths(),
                    info.corrosion_ca_certificate_pem.as_ref().unwrap(),
                ),
            ] {
                assert_eq!(fs::read_to_string(&paths.ca_certificate).unwrap(), *pin);
                assert_eq!(X509::stack_from_pem(pin.as_bytes()).unwrap().len(), 1);
                for (cert_path, key_path) in [
                    (&paths.ca_certificate, &paths.ca_private_key),
                    (&paths.certificate, &paths.private_key),
                ] {
                    let cert = X509::from_pem(&fs::read(cert_path).unwrap()).unwrap();
                    let key = PKey::private_key_from_pem(&fs::read(key_path).unwrap()).unwrap();
                    assert!(cert.public_key().unwrap().public_eq(&key));
                    assert!(public_keys.insert(key.public_key_to_der().unwrap()));
                }
                let ca = X509::from_pem(pin.as_bytes()).unwrap();
                assert!(ca.verify(&ca.public_key().unwrap()).unwrap());
                let chain = X509::stack_from_pem(&fs::read(&paths.fullchain).unwrap()).unwrap();
                assert_eq!(chain.len(), 2);
                assert_eq!(chain[1].to_pem().unwrap(), pin.as_bytes());
                assert_ne!(chain[0].to_pem().unwrap(), pin.as_bytes());
            }
            assert_eq!(
                info.https_spki_sha256,
                info.https_spki_fingerprint().unwrap()
            );
            let metadata = fs::read_to_string(
                identity
                    .tls_paths()
                    .certificate
                    .parent()
                    .unwrap()
                    .join(METADATA),
            )
            .unwrap();
            assert!(!metadata.contains("PRIVATE KEY"));
            assert!(!metadata.contains(identity.keys().wireguard_private_key().unwrap().as_str()));
            let debug = format!("{identity:?}");
            assert!(!debug.contains(identity.keys().wireguard_private_key().unwrap().as_str()));
        }
    }

    #[test]
    fn challenge_rejects_other_requests_networks_and_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let identity = prepare(&temp.path().join("first")).unwrap();
        let other = prepare(&temp.path().join("other")).unwrap();
        let network = identity.prepared.config.network_id.clone();
        let (challenge, _) =
            WgChallenge::begin(other.request(), network.clone(), 100, 200).unwrap();
        assert!(identity.answer_challenge(&challenge, 101).is_err());
        let (mut challenge, _) = WgChallenge::begin(identity.request(), network, 100, 200).unwrap();
        assert!(identity.answer_challenge(&challenge, 200).is_err());
        challenge.network_id = NetworkId::random().unwrap();
        assert!(identity.answer_challenge(&challenge, 101).is_err());
    }

    #[test]
    fn leaf_renewal_with_the_local_ca_preserves_public_pins() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let identity = prepare(temp.path()).unwrap();
        for paths in [identity.tls_paths(), identity.gossip_paths()] {
            let ca_key = fs::read(&paths.ca_private_key).unwrap();
            let old_leaf = fs::read(&paths.certificate).unwrap();
            let output = std::process::Command::new("openssl")
                .args(["x509", "-in"])
                .arg(&paths.certificate)
                .arg("-CA")
                .arg(&paths.ca_certificate)
                .arg("-CAkey")
                .arg(&paths.ca_private_key)
                .args(["-set_serial", "99", "-days", "364"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_ne!(output.stdout, old_leaf);
            fs::write(&paths.certificate, &output.stdout).unwrap();
            fs::write(
                &paths.fullchain,
                [output.stdout, fs::read(&paths.ca_certificate).unwrap()].concat(),
            )
            .unwrap();
            assert_eq!(fs::read(&paths.ca_private_key).unwrap(), ca_key);
        }
        let renewed = prepare(temp.path()).unwrap();
        assert_eq!(identity.request(), renewed.request());
        assert_eq!(identity.server_info(), renewed.server_info());
    }

    #[test]
    fn public_ipv6_endpoint_is_a_verified_ip_san() {
        let temp = tempfile::tempdir().unwrap();
        let identity = ServerIdentity::prepare(
            temp.path(),
            NetworkId::random().unwrap(),
            "ipv6-server".parse().unwrap(),
            "[2001:db8::42]:51820".parse().unwrap(),
            host_key(),
            22,
            443,
        )
        .unwrap();
        for paths in [identity.tls_paths(), identity.gossip_paths()] {
            assert!(openssl_verify(
                paths,
                "sslserver",
                "-verify_ip",
                "2001:db8::42"
            ));
        }
    }
    fn snapshot(path: &Path) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
        std::fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect()
    }

    fn prepare_config(path: &Path, config: Config) -> Result<ServerIdentity> {
        ServerIdentity::prepare(
            path,
            config.network_id,
            config.name,
            config.wireguard_endpoint,
            config.ssh_host_public_key,
            config.ssh_port,
            config.https_port,
        )
    }

    #[test]
    fn every_configuration_change_is_explicit_and_never_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let identity = prepare(temp.path()).unwrap();
        let original = snapshot(temp.path());
        for change in 0..6 {
            let mut config = identity.prepared.config.clone();
            match change {
                0 => config.network_id = NetworkId::random().unwrap(),
                1 => config.name = "other-server".parse().unwrap(),
                2 => config.wireguard_endpoint = "192.0.2.99:51820".parse().unwrap(),
                3 => {
                    config.ssh_host_public_key = LocalDeviceKeys::generate()
                        .unwrap()
                        .ssh_public_key()
                        .unwrap()
                }
                4 => config.ssh_port = 2222,
                5 => config.https_port = 8443,
                _ => unreachable!(),
            }
            let error = prepare_config(temp.path(), config).unwrap_err();
            assert!(error.to_string().contains("configuration mismatch"));
            assert_eq!(snapshot(temp.path()), original);
        }
    }

    #[test]
    fn invalid_endpoints_fail_before_creating_any_files() {
        let temp = tempfile::tempdir().unwrap();
        let seed = prepare(&temp.path().join("seed")).unwrap();
        for (index, endpoint) in [
            "127.0.0.1:51820",
            "0.0.0.0:51820",
            "192.0.2.1:0",
            "[ff02::1]:51820",
        ]
        .into_iter()
        .enumerate()
        {
            let mut config = seed.prepared.config.clone();
            config.wireguard_endpoint = endpoint.parse().unwrap();
            let path = temp.path().join(index.to_string());
            assert!(prepare_config(&path, config).is_err());
            assert!(!path.exists());
        }
        for https in [true, false] {
            let mut config = seed.prepared.config.clone();
            if https {
                config.https_port = 0;
            } else {
                config.ssh_port = 0;
            }
            let path = temp.path().join("bad-port");
            assert!(prepare_config(&path, config).is_err());
            assert!(!path.exists());
        }
    }

    #[test]
    fn missing_files_and_interrupted_prepare_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let seed = prepare(&temp.path().join("seed")).unwrap();
        let files = snapshot(seed.tls_paths().certificate.parent().unwrap());
        for missing in files.keys().filter(|name| *name != "prepare-started") {
            let case = tempfile::tempdir().unwrap();
            prepare(case.path()).unwrap();
            std::fs::remove_file(case.path().join(missing)).unwrap();
            let before = snapshot(case.path());
            assert!(prepare(case.path()).is_err(), "missing {missing:?}");
            assert!(ServerIdentity::load(case.path()).is_err());
            assert_eq!(snapshot(case.path()), before);
        }
        for filename in [
            "prepare-started",
            DEVICE_KEYS,
            "https-ca-key.pem",
            "unrelated",
        ] {
            let case = tempfile::tempdir().unwrap();
            std::fs::write(case.path().join(filename), b"do not replace").unwrap();
            let before = snapshot(case.path());
            assert!(prepare(case.path()).is_err());
            assert_eq!(snapshot(case.path()), before);
        }
    }

    #[test]
    fn metadata_is_bounded_strict_and_bound_to_keys_and_pins() {
        let temp = tempfile::tempdir().unwrap();
        let identity = prepare(temp.path()).unwrap();
        let original = serde_json::to_value(&identity.prepared).unwrap();
        let mut unknown = original.clone();
        unknown["private_key_path"] = "../../victim".into();
        let mut version = original.clone();
        version["version"] = 2.into();
        let mut kind = original.clone();
        kind["request"]["kind"] = "laptop".into();
        let mut pin = original.clone();
        pin["server_info"]["https_certificate_pem"] =
            original["server_info"]["corrosion_ca_certificate_pem"].clone();
        let mut endpoint = original.clone();
        endpoint["server_info"]["https_port"] = 8443.into();
        let mut cases = vec![b"{".to_vec(), vec![b' '; MAX_FILE_BYTES + 1]];
        cases.extend(
            [unknown, version, kind, pin, endpoint]
                .iter()
                .map(|v| serde_json::to_vec(v).unwrap()),
        );
        for bytes in cases {
            std::fs::write(temp.path().join(METADATA), bytes).unwrap();
            let before = snapshot(temp.path());
            assert!(prepare(temp.path()).is_err());
            assert!(ServerIdentity::load(temp.path()).is_err());
            assert_eq!(snapshot(temp.path()), before);
        }
    }

    #[test]
    fn rejects_substituted_device_and_tls_keys() {
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("first");
        let other_path = temp.path().join("other");
        let first = prepare(&first_path).unwrap();
        let other = prepare(&other_path).unwrap();
        for (target, substitute) in [
            (first_path.join(DEVICE_KEYS), other_path.join(DEVICE_KEYS)),
            (
                first.tls_paths().private_key.clone(),
                other.tls_paths().private_key.clone(),
            ),
            (
                first.gossip_paths().ca_private_key.clone(),
                other.gossip_paths().ca_private_key.clone(),
            ),
            (
                first.tls_paths().certificate.clone(),
                other.tls_paths().certificate.clone(),
            ),
        ] {
            let original = std::fs::read(&target).unwrap();
            std::fs::write(&target, std::fs::read(substitute).unwrap()).unwrap();
            let before = snapshot(&first_path);
            assert!(prepare(&first_path).is_err());
            assert_eq!(snapshot(&first_path), before);
            std::fs::write(target, original).unwrap();
        }
    }

    #[test]
    fn directories_reject_permissions_symlinks_and_parent_traversal() {
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
        };
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        let identity = prepare(&private).unwrap();
        let before = snapshot(&private);
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare(&private).is_err());
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = temp.path().join("alias");
        symlink(&private, &alias).unwrap();
        assert!(prepare(&alias).is_err());
        assert!(ServerIdentity::load(&alias).is_err());
        assert!(prepare(&alias.join("new-identity")).is_err());
        assert!(!private.join("new-identity").exists());
        assert!(prepare(&private.join("..").join("traversal")).is_err());
        assert!(!temp.path().join("traversal").exists());
        assert_eq!(snapshot(&private), before);
        assert_eq!(prepare(&private).unwrap().request(), identity.request());
    }

    #[test]
    fn files_require_regular_single_link_owned_0600_and_no_symlinks() {
        use std::{
            fs,
            os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        };
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("identity");
        prepare(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
        for name in snapshot(&path)
            .keys()
            .filter(|name| *name != "prepare-started")
        {
            let file = path.join(name);
            let meta = fs::metadata(&file).unwrap();
            assert_eq!(meta.mode() & 0o7777, 0o600);
            assert_eq!(meta.uid(), rustix::process::geteuid().as_raw());
            assert_eq!(meta.nlink(), 1);
            let original = fs::read(&file).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(prepare(&path).is_err(), "permissions {name:?}");
            assert!(ServerIdentity::load(&path).is_err());
            assert_eq!(fs::read(&file).unwrap(), original);
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            let link = temp.path().join("hardlink");
            fs::hard_link(&file, &link).unwrap();
            assert!(prepare(&path).is_err(), "hardlink {name:?}");
            assert!(ServerIdentity::load(&path).is_err());
            fs::remove_file(&link).unwrap();
            let saved = temp.path().join("saved");
            fs::rename(&file, &saved).unwrap();
            symlink(&saved, &file).unwrap();
            assert!(prepare(&path).is_err(), "symlink {name:?}");
            assert!(ServerIdentity::load(&path).is_err());
            assert_eq!(fs::read(&saved).unwrap(), original);
            fs::remove_file(&file).unwrap();
            fs::create_dir(&file).unwrap();
            assert!(prepare(&path).is_err(), "directory {name:?}");
            fs::remove_dir(&file).unwrap();
            fs::rename(&saved, &file).unwrap();
        }
        prepare(&path).unwrap();
    }

    #[test]
    fn concurrent_prepare_never_replaces_the_winners_identity() {
        let temp = tempfile::tempdir().unwrap();
        let barrier = std::sync::Barrier::new(4);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        prepare(temp.path())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        let winners: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
        assert!(!winners.is_empty());
        let reloaded = prepare(temp.path()).unwrap();
        for winner in winners {
            assert_eq!(winner.request(), reloaded.request());
            assert_eq!(winner.server_info(), reloaded.server_info());
        }
    }
}
