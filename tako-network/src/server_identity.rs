//! Local-only server preparation. Public HTTPS pins identify a per-server CA
//! issuer, not a leaf: clients verify its chain and the endpoint DNS name/IP.
//! Admission, public snapshot publication and listeners belong to the caller. CA private keys must never be included in enrollment or profiles.

use std::{
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use openssl::{
    asn1::{Asn1Time, Asn1TimeRef},
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
    DeviceKind, DeviceName, EnrollmentRequest, KeyFingerprint, LocalDeviceKeys, NetworkId, Result,
    ServerInfo, SshPublicKey, WgChallenge, WgProof, error::invalid,
};

const METADATA: &str = "identity-prepared.json";
const DEVICE_KEYS: &str = "device-keys.json";
const MAX_FILE_BYTES: usize = 64 * 1024;
const RENEWAL_JOURNAL: &str = "leaf-renewal.json";
const RENEWAL_STAGE: &str = ".leaf-renewal-stage";
const DAY: i64 = 24 * 60 * 60;
const RENEW_BEFORE: i64 = 30 * DAY;
const LEAF_LIFETIME: i64 = 365 * DAY;

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
        dir.lock()?;
        let bytes = dir
            .read_optional(METADATA)?
            .ok_or_else(|| invalid("server identity", "identity is not prepared"))?;
        Self::load_prepared(&directory, &dir, Prepared::parse(&bytes)?)
    }

    /// Recover interrupted renewal, then renew leaves with at most 30 days left.
    /// Existing CA, leaf and device keys and public pins are retained. Expired
    /// leaves are accepted only after verifying their bindings and a currently
    /// valid CA. Requires prepared state; never creates missing identities.
    pub fn renew(directory: impl AsRef<Path>) -> Result<Self> {
        Self::renew_with_clock(directory.as_ref(), current_time)
    }

    #[cfg(all(test, unix))]
    fn renew_at(directory: &Path, now: i64) -> Result<Self> {
        Self::renew_with_clock(directory, || Ok(now))
    }

    fn renew_with_clock(directory: &Path, clock: impl Fn() -> Result<i64>) -> Result<Self> {
        let directory = std::path::absolute(directory)?;
        let dir = secure::Directory::open(&directory, false)?;
        dir.lock()?;
        let now = clock()?;
        let bytes = dir
            .read_optional(METADATA)?
            .ok_or_else(|| invalid("server identity", "identity is not prepared"))?;
        let identity = Self::read_identity(&directory, Prepared::parse(&bytes)?)?;
        let digest = KeyFingerprint::of_bytes(&bytes);
        if let Some(bytes) = dir.read_optional(RENEWAL_JOURNAL)? {
            let renewal: Renewal = serde_json::from_slice(&bytes)
                .map_err(|_| invalid("leaf renewal", "invalid journal"))?;
            if renewal.version != 1 || renewal.prepared_digest != digest {
                return Err(invalid(
                    "leaf renewal",
                    "journal does not bind prepared identity",
                ));
            }
            let materials =
                identity.validate_at(&dir, now, LeafValidation::Renewal, Some(&renewal))?;
            dir.check_path(&directory)?;
            dir.discard_stage()?;
            renewal.https.apply(&dir, &identity.tls, &materials[0].ca)?;
            renewal
                .gossip
                .apply(&dir, &identity.gossip, &materials[1].ca)?;
            dir.remove_journal()?;
        }
        let materials = identity.validate_at(&dir, now, LeafValidation::Renewal, None)?;
        dir.check_path(&directory)?;
        dir.discard_stage()?;
        let names = certificate_names(&identity.prepared.config, identity.request())?;
        let renewal = Renewal {
            version: 1,
            prepared_digest: digest,
            https: LeafUpdate::plan(&materials[0], &names, false, now)?,
            gossip: LeafUpdate::plan(&materials[1], &names, true, now)?,
        };
        if renewal.https.previous_pem != renewal.https.replacement_pem
            || renewal.gossip.previous_pem != renewal.gossip.replacement_pem
        {
            dir.publish_journal(&serde_json::to_vec(&renewal)?)?;
            renewal.https.apply(&dir, &identity.tls, &materials[0].ca)?;
            renewal
                .gossip
                .apply(&dir, &identity.gossip, &materials[1].ca)?;
            dir.remove_journal()?;
        }
        identity.validate_at(&dir, clock()?, LeafValidation::Current, None)?;
        dir.check_path(&directory)?;
        Ok(identity)
    }

    fn load_prepared(
        directory: &Path,
        dir: &secure::Directory,
        prepared: Prepared,
    ) -> Result<Self> {
        if dir.read_optional(RENEWAL_JOURNAL)?.is_some() {
            return Err(invalid(
                "server identity",
                "pending leaf renewal: call renew to recover",
            ));
        }
        let identity = Self::read_identity(directory, prepared)?;
        identity.validate(dir)?;
        dir.check_path(directory)?;
        Ok(identity)
    }

    fn read_identity(directory: &Path, prepared: Prepared) -> Result<Self> {
        let keys_path = directory.join(DEVICE_KEYS);
        Ok(Self {
            keys: LocalDeviceKeys::load(&keys_path)?,
            keys_path,
            prepared,
            tls: ServerTlsPaths::new(directory, "https"),
            gossip: ServerTlsPaths::new(directory, "gossip"),
        })
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
        dir.lock()?;
        if let Some(bytes) = dir.read_optional(METADATA)? {
            let prepared = Prepared::parse(&bytes)?;
            if prepared.config != config {
                return Err(invalid(
                    "server identity",
                    "network or configuration mismatch",
                ));
            }
            drop(dir);
            let identity = Self::renew(&directory)?;
            if identity.prepared.config != config {
                return Err(invalid(
                    "server identity",
                    "configuration changed during renewal",
                ));
            }
            return Ok(identity);
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
        self.validate_at(dir, current_time()?, LeafValidation::Current, None)
            .map(|_| ())
    }

    fn validate_at(
        &self,
        dir: &secure::Directory,
        now: i64,
        policy: LeafValidation,
        renewal: Option<&Renewal>,
    ) -> Result<[TlsMaterial; 2]> {
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
        let https = validate_tls(
            dir,
            &self.tls,
            &names,
            false,
            now,
            policy,
            renewal.map(|r| &r.https),
        )?;
        let gossip = validate_tls(
            dir,
            &self.gossip,
            &names,
            true,
            now,
            policy,
            renewal.map(|r| &r.gossip),
        )?;
        expected.https_certificate_pem = Some(pem_string(https.ca.to_pem()?)?);
        expected.corrosion_ca_certificate_pem = Some(pem_string(gossip.ca.to_pem()?)?);
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
        Ok([https, gossip])
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
    let now = current_time()?;
    let leaf = issue_leaf(
        &ca,
        &ca_key,
        &leaf_key,
        names,
        gossip,
        now,
        (now + LEAF_LIFETIME).min(timestamp(ca.not_after())?),
    )?;
    let ca_pem = ca.to_pem()?;
    let leaf_pem = leaf.to_pem()?;
    let ca_secret = Zeroizing::new(ca_key.private_key_to_pem_pkcs8()?);
    let leaf_secret = Zeroizing::new(leaf_key.private_key_to_pem_pkcs8()?);
    dir.write_path(&paths.ca_private_key, &ca_secret)?;
    dir.write_path(&paths.private_key, &leaf_secret)?;
    dir.write_path(&paths.ca_certificate, &ca_pem)?;
    dir.write_path(&paths.certificate, &leaf_pem)?;
    dir.write_path(&paths.fullchain, &[leaf_pem, ca_pem.clone()].concat())?;
    pem_string(ca_pem)
}

fn issue_leaf(
    ca: &X509,
    ca_key: &PKey<Private>,
    leaf_key: &PKey<Private>,
    names: &CertificateNames,
    gossip: bool,
    now: i64,
    expires_at: i64,
) -> Result<X509> {
    let mut leaf = certificate(
        leaf_key,
        if gossip { "Tako gossip" } else { "Tako HTTPS" },
        365,
    )?;
    let start = Asn1Time::from_unix(now)?;
    let end = Asn1Time::from_unix(expires_at)?;
    leaf.set_not_before(&start)?;
    leaf.set_not_after(&end)?;
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
    let san = san.build(&leaf.x509v3_context(Some(ca), None))?;
    leaf.append_extension(san)?;
    let aki = AuthorityKeyIdentifier::new()
        .keyid(true)
        .build(&leaf.x509v3_context(Some(ca), None))?;
    leaf.append_extension(aki)?;
    let ski = SubjectKeyIdentifier::new().build(&leaf.x509v3_context(Some(ca), None))?;
    leaf.append_extension(ski)?;
    leaf.sign(ca_key, MessageDigest::sha256())?;
    Ok(leaf.build())
}

fn pem_string(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| invalid("certificate", "invalid PEM encoding"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeafValidation {
    Current,
    // CA validity is always checked at `now`, even when checking an old leaf.
    Renewal,
}

struct TlsMaterial {
    ca: X509,
    ca_key: PKey<Private>,
    leaf: X509,
    leaf_key: PKey<Private>,
}

fn current_time() -> Result<i64> {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_secs().try_into().ok())
        .ok_or_else(|| invalid("server TLS", "invalid system clock"))
}

fn timestamp(time: &Asn1TimeRef) -> Result<i64> {
    let diff = Asn1Time::from_unix(0)?.diff(time)?;
    Ok(i64::from(diff.days) * DAY + i64::from(diff.secs))
}

#[allow(clippy::too_many_arguments)]
fn validate_tls(
    dir: &secure::Directory,
    paths: &ServerTlsPaths,
    names: &CertificateNames,
    gossip: bool,
    now: i64,
    policy: LeafValidation,
    update: Option<&LeafUpdate>,
) -> Result<TlsMaterial> {
    let ca_bytes = dir.read_path(&paths.ca_certificate)?;
    let leaf_bytes = dir.read_path(&paths.certificate)?;
    let fullchain = dir.read_path(&paths.fullchain)?;
    let ca = X509::from_pem(&ca_bytes)?;
    if ca.to_pem()? != *ca_bytes {
        return Err(invalid(
            "server TLS",
            "expected one canonical CA certificate",
        ));
    }
    if timestamp(ca.not_before())? > now || timestamp(ca.not_after())? <= now {
        return Err(invalid("server TLS", "CA is expired or not yet valid"));
    }
    let ca_key = PKey::private_key_from_pem(&dir.read_path(&paths.ca_private_key)?)?;
    let leaf_key = PKey::private_key_from_pem(&dir.read_path(&paths.private_key)?)?;
    if !ca.public_key()?.public_eq(&ca_key)
        || ca_key.public_eq(&leaf_key)
        || !ca.verify(&ca_key)?
        || ca.subject_name().to_der()? != ca.issuer_name().to_der()?
    {
        return Err(invalid("server TLS", "certificate/key or issuer mismatch"));
    }
    let validate = |pem: &[u8]| validate_leaf(&ca, &leaf_key, pem, names, gossip, now, policy);
    let leaf = if let Some(update) = update {
        validate(update.previous_pem.as_bytes())?;
        let replacement = validate(update.replacement_pem.as_bytes())?;
        let matches_leaf = [&update.previous_pem, &update.replacement_pem]
            .iter()
            .any(|pem| pem.as_bytes() == leaf_bytes.as_slice());
        let matches_chain = [&update.previous_pem, &update.replacement_pem]
            .iter()
            .any(|pem| [pem.as_bytes(), ca_bytes.as_slice()].concat() == *fullchain);
        if !matches_leaf || !matches_chain {
            return Err(invalid(
                "leaf renewal",
                "files do not match journal old/new certificates",
            ));
        }
        if timestamp(replacement.not_after())?
            < timestamp(X509::from_pem(update.previous_pem.as_bytes())?.not_after())?
        {
            return Err(invalid(
                "leaf renewal",
                "journal would shorten leaf validity",
            ));
        }
        replacement
    } else {
        if *fullchain != [leaf_bytes.as_slice(), ca_bytes.as_slice()].concat() {
            return Err(invalid(
                "server TLS",
                "expected matching certificate and fullchain",
            ));
        }
        validate(&leaf_bytes)?
    };
    Ok(TlsMaterial {
        ca,
        ca_key,
        leaf,
        leaf_key,
    })
}

fn validate_leaf(
    ca: &X509,
    key: &PKey<Private>,
    bytes: &[u8],
    names: &CertificateNames,
    gossip: bool,
    now: i64,
    policy: LeafValidation,
) -> Result<X509> {
    let leaf = X509::from_pem(bytes)?;
    if leaf.to_pem()? != bytes || !leaf.public_key()?.public_eq(key) {
        return Err(invalid(
            "server TLS",
            "leaf encoding or private key mismatch",
        ));
    }
    validate_leaf_roles(&leaf, gossip)?;
    let start = timestamp(leaf.not_before())?;
    let end = timestamp(leaf.not_after())?;
    if start > now || end <= start || (policy == LeafValidation::Current && end <= now) {
        return Err(invalid("server TLS", "invalid leaf validity interval"));
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
    // The issuer was checked at the real current time above. An expired leaf
    // must still pass strict verification at a time within its own validity.
    let verify_at = now.min(end - 1);
    if !verify_leaf(
        ca,
        &leaf,
        X509PurposeId::SSL_SERVER,
        &names.hostname,
        verify_at,
    )? || verify_leaf(
        ca,
        &leaf,
        X509PurposeId::SSL_CLIENT,
        &names.hostname,
        verify_at,
    )? != gossip
    {
        return Err(invalid(
            "server TLS",
            "chain, validity, name or purpose mismatch",
        ));
    }
    Ok(leaf)
}

fn validate_leaf_roles(leaf: &X509, gossip: bool) -> Result<()> {
    // OpenSSL purpose checks allow absent EKU (unrestricted use). Require the
    // explicit managed-leaf extensions as well. Inspect only OpenSSL's own DER
    // output, then leave signature/chain validation to OpenSSL below.
    let mut eku = ExtendedKeyUsage::new();
    eku.server_auth();
    if gossip {
        eku.client_auth();
    }
    let expected_eku = eku.build()?.to_der()?;
    let expected_constraints = BasicConstraints::new().critical().build()?.to_der()?;
    let der = leaf.to_der()?;
    let (_, mut certificate) = der_item(&mut der.as_slice())?;
    let (_, mut tbs) = der_item(&mut certificate)?;
    let mut roles = false;
    let mut constraints = false;
    while !tbs.is_empty() {
        let (tag, mut value) = der_item(&mut tbs)?;
        if tag != 0xa3 {
            continue;
        }
        let (_, mut extensions) = der_item(&mut value)?;
        while !extensions.is_empty() {
            let encoded = extensions;
            let (_, mut extension) = der_item(&mut extensions)?;
            let encoded = &encoded[..encoded.len() - extensions.len()];
            let (_, oid) = der_item(&mut extension)?;
            match oid {
                [0x55, 0x1d, 0x25] if !roles && encoded == expected_eku => roles = true,
                [0x55, 0x1d, 0x13] if !constraints && encoded == expected_constraints => {
                    constraints = true
                }
                [0x55, 0x1d, 0x25] | [0x55, 0x1d, 0x13] => {
                    return Err(invalid(
                        "server TLS",
                        "unexpected leaf roles or CA constraints",
                    ));
                }
                _ => {}
            }
        }
    }
    if !roles || !constraints {
        return Err(invalid(
            "server TLS",
            "missing explicit leaf roles or CA constraints",
        ));
    }
    Ok(())
}

// Minimal bounds-checked TLV traversal for the already parsed/re-encoded X509.
fn der_item<'a>(input: &mut &'a [u8]) -> Result<(u8, &'a [u8])> {
    let error = || invalid("server TLS", "invalid certificate DER");
    let (&tag, rest) = input.split_first().ok_or_else(error)?;
    let (&length, mut rest) = rest.split_first().ok_or_else(error)?;
    let length = if length < 128 {
        usize::from(length)
    } else {
        let count = usize::from(length & 0x7f);
        if count == 0 || count > 4 {
            return Err(error());
        }
        let bytes = rest.get(..count).ok_or_else(error)?;
        rest = &rest[count..];
        bytes
            .iter()
            .try_fold(0usize, |n, b| {
                n.checked_mul(256)?.checked_add(usize::from(*b))
            })
            .ok_or_else(error)?
    };
    let content = rest.get(..length).ok_or_else(error)?;
    *input = &rest[length..];
    Ok((tag, content))
}

fn verify_leaf(
    ca: &X509,
    leaf: &X509,
    purpose: X509PurposeId,
    hostname: &str,
    now: i64,
) -> Result<bool> {
    let mut store = X509StoreBuilder::new()?;
    store.add_cert(ca.clone())?;
    let mut param = X509VerifyParam::new()?;
    param.set_flags(X509VerifyFlags::X509_STRICT)?;
    param.set_purpose(purpose)?;
    param.set_host(hostname)?;
    // OpenSSL time_t can be narrower than i64 on other supported platforms.
    #[allow(clippy::useless_conversion)]
    let time = now
        .try_into()
        .map_err(|_| invalid("server TLS", "time out of range"))?;
    param.set_time(time);
    store.set_param(&param)?;
    let chain = Stack::new()?;
    Ok(
        X509StoreContext::new()?.init(&store.build(), leaf, &chain, |context| {
            context.verify_cert()
        })?,
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Renewal {
    version: u32,
    prepared_digest: KeyFingerprint,
    https: LeafUpdate,
    gossip: LeafUpdate,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeafUpdate {
    previous_pem: String,
    replacement_pem: String,
}

impl LeafUpdate {
    fn plan(
        material: &TlsMaterial,
        names: &CertificateNames,
        gossip: bool,
        now: i64,
    ) -> Result<Self> {
        let previous_pem = pem_string(material.leaf.to_pem()?)?;
        let expires = timestamp(material.leaf.not_after())?;
        let new_expiry = (now + LEAF_LIFETIME).min(timestamp(material.ca.not_after())?);
        let replacement_pem = if expires <= now + RENEW_BEFORE && expires < new_expiry {
            let leaf = issue_leaf(
                &material.ca,
                &material.ca_key,
                &material.leaf_key,
                names,
                gossip,
                now,
                new_expiry,
            )?;
            let pem = leaf.to_pem()?;
            validate_leaf(
                &material.ca,
                &material.leaf_key,
                &pem,
                names,
                gossip,
                now,
                LeafValidation::Current,
            )?;
            pem_string(pem)?
        } else {
            previous_pem.clone()
        };
        Ok(Self {
            previous_pem,
            replacement_pem,
        })
    }

    fn apply(&self, dir: &secure::Directory, paths: &ServerTlsPaths, ca: &X509) -> Result<()> {
        dir.replace_path(&paths.certificate, self.replacement_pem.as_bytes())?;
        dir.replace_path(
            &paths.fullchain,
            &[self.replacement_pem.as_bytes(), &ca.to_pem()?].concat(),
        )
    }
}

fn renewal_boundary() -> Result<()> {
    #[cfg(all(test, unix))]
    tests::renewal_boundary()?;
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
        // Lock the directory inode itself: no replaceable lock-file path, and
        // read-only load acquires the same lock without creating any files.
        pub(super) fn lock(&self) -> Result<()> {
            fs::flock(&self.0, fs::FlockOperation::LockExclusive)?;
            Ok(())
        }

        pub(super) fn discard_stage(&self) -> Result<()> {
            if self.read_optional(RENEWAL_STAGE)?.is_some() {
                fs::unlinkat(&self.0, RENEWAL_STAGE, fs::AtFlags::empty())?;
                renewal_boundary()?;
                fs::fsync(&self.0)?;
                renewal_boundary()?;
            }
            Ok(())
        }

        pub(super) fn publish_journal(&self, bytes: &[u8]) -> Result<()> {
            if self.read_optional(RENEWAL_JOURNAL)?.is_some() {
                return Err(invalid("leaf renewal", "journal already exists"));
            }
            self.write_atomic(RENEWAL_JOURNAL, bytes)
        }

        pub(super) fn replace_path(&self, path: &Path, bytes: &[u8]) -> Result<()> {
            if self.read_path(path)?.as_slice() != bytes {
                self.write_atomic(filename(path)?, bytes)?;
            }
            Ok(())
        }

        fn write_atomic(&self, name: &str, bytes: &[u8]) -> Result<()> {
            if bytes.len() > MAX_FILE_BYTES {
                return Err(invalid("identity file", "too large"));
            }
            let fd = fs::openat(
                &self.0,
                RENEWAL_STAGE,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?;
            let mut file = File::from(fd);
            fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
            renewal_boundary()?;
            file.write_all(bytes)?;
            renewal_boundary()?;
            file.sync_all()?;
            renewal_boundary()?;
            fs::renameat(&self.0, RENEWAL_STAGE, &self.0, name)?;
            renewal_boundary()?;
            fs::fsync(&self.0)?;
            renewal_boundary()?;
            Ok(())
        }

        pub(super) fn remove_journal(&self) -> Result<()> {
            // Recheck protected-file invariants before unlinking.
            self.read_optional(RENEWAL_JOURNAL)?
                .ok_or_else(|| invalid("leaf renewal", "missing journal"))?;
            fs::unlinkat(&self.0, RENEWAL_JOURNAL, fs::AtFlags::empty())?;
            renewal_boundary()?;
            fs::fsync(&self.0)?;
            renewal_boundary()?;
            Ok(())
        }

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
        pub(super) fn lock(&self) -> Result<()> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn discard_stage(&self) -> Result<()> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn publish_journal(&self, _: &[u8]) -> Result<()> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn replace_path(&self, _: &Path, _: &[u8]) -> Result<()> {
            Err(crate::Error::UnsupportedPlatform)
        }
        pub(super) fn remove_journal(&self) -> Result<()> {
            Err(crate::Error::UnsupportedPlatform)
        }
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

    thread_local! {
        static RENEWAL_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

    pub(super) fn renewal_boundary() -> Result<()> {
        RENEWAL_FAILURE.with(|remaining| match remaining.get() {
            Some(0) => {
                remaining.set(None);
                Err(std::io::Error::other("injected renewal interruption").into())
            }
            Some(n) => {
                remaining.set(Some(n - 1));
                Ok(())
            }
            None => Ok(()),
        })
    }

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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
        let identity = prepare(temp.path()).unwrap();
        let names = certificate_names(&identity.prepared.config, identity.request()).unwrap();
        for (paths, gossip) in [
            (identity.tls_paths(), false),
            (identity.gossip_paths(), true),
        ] {
            let ca_key_pem = fs::read(&paths.ca_private_key).unwrap();
            let old_leaf = fs::read(&paths.certificate).unwrap();
            // An external renewal re-signs the existing leaf key with the local
            // CA under a new serial and validity. (OpenSSL 3.0's `x509 -CA`
            // refuses to re-sign a CA-issued certificate, so build it here.)
            let ca =
                openssl::x509::X509::from_pem(&fs::read(&paths.ca_certificate).unwrap()).unwrap();
            let ca_key = openssl::pkey::PKey::private_key_from_pem(&ca_key_pem).unwrap();
            let leaf_key =
                openssl::pkey::PKey::private_key_from_pem(&fs::read(&paths.private_key).unwrap())
                    .unwrap();
            let now = current_time().unwrap();
            let renewed = issue_leaf(
                &ca,
                &ca_key,
                &leaf_key,
                &names,
                gossip,
                now,
                now + 364 * 86_400,
            )
            .unwrap();
            let leaf_pem = renewed.to_pem().unwrap();
            assert_ne!(leaf_pem, old_leaf);
            fs::write(&paths.certificate, &leaf_pem).unwrap();
            fs::write(
                &paths.fullchain,
                [leaf_pem, fs::read(&paths.ca_certificate).unwrap()].concat(),
            )
            .unwrap();
            assert_eq!(fs::read(&paths.ca_private_key).unwrap(), ca_key_pem);
        }
        let renewed = prepare(temp.path()).unwrap();
        assert_eq!(identity.request(), renewed.request());
        assert_eq!(identity.server_info(), renewed.server_info());
    }

    #[test]
    fn public_ipv6_endpoint_is_a_verified_ip_san() {
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
        let seed = prepare(&temp.path().join("seed")).unwrap();
        let files = snapshot(seed.tls_paths().certificate.parent().unwrap());
        for missing in files.keys().filter(|name| *name != "prepare-started") {
            let case = crate::private_tempdir();
            prepare(case.path()).unwrap();
            std::fs::remove_file(case.path().join(missing)).unwrap();
            let before = snapshot(case.path());
            assert!(prepare(case.path()).is_err(), "missing {missing:?}");
            assert!(ServerIdentity::load(case.path()).is_err());
            assert!(ServerIdentity::renew(case.path()).is_err());
            assert_eq!(snapshot(case.path()), before);
        }
        for filename in [
            "prepare-started",
            DEVICE_KEYS,
            "https-ca-key.pem",
            "unrelated",
        ] {
            let case = crate::private_tempdir();
            std::fs::write(case.path().join(filename), b"do not replace").unwrap();
            let before = snapshot(case.path());
            assert!(prepare(case.path()).is_err());
            assert_eq!(snapshot(case.path()), before);
        }
    }

    #[test]
    fn metadata_is_bounded_strict_and_bound_to_keys_and_pins() {
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        let temp = crate::private_tempdir();
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
        assert!(ServerIdentity::renew(&alias).is_err());
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
        let temp = crate::private_tempdir();
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
            assert!(ServerIdentity::renew(&path).is_err());
            assert_eq!(fs::read(&file).unwrap(), original);
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            let link = temp.path().join("hardlink");
            fs::hard_link(&file, &link).unwrap();
            assert!(prepare(&path).is_err(), "hardlink {name:?}");
            assert!(ServerIdentity::load(&path).is_err());
            assert!(ServerIdentity::renew(&path).is_err());
            fs::remove_file(&link).unwrap();
            let saved = temp.path().join("saved");
            fs::rename(&file, &saved).unwrap();
            symlink(&saved, &file).unwrap();
            assert!(prepare(&path).is_err(), "symlink {name:?}");
            assert!(ServerIdentity::load(&path).is_err());
            assert!(ServerIdentity::renew(&path).is_err());
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
        let temp = crate::private_tempdir();
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
    fn materials(identity: &ServerIdentity) -> [TlsMaterial; 2] {
        let dir = secure::Directory::open(identity.keys_path().parent().unwrap(), false).unwrap();
        identity
            .validate_at(&dir, current_time().unwrap(), LeafValidation::Current, None)
            .unwrap()
    }

    fn assert_immutable_files(
        before: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
        path: &Path,
    ) {
        for (name, bytes) in before {
            if !matches!(
                name.to_str().unwrap(),
                "https-leaf.pem"
                    | "https-fullchain.pem"
                    | "gossip-leaf.pem"
                    | "gossip-fullchain.pem"
                    | RENEWAL_JOURNAL
                    | RENEWAL_STAGE
            ) {
                assert_eq!(
                    &std::fs::read(path.join(name)).unwrap(),
                    bytes,
                    "changed {name:?}"
                );
            }
        }
    }

    #[test]
    fn native_renewal_preserves_keys_pins_and_renews_expiring_or_expired_leaves() {
        use std::{fs, os::unix::fs::MetadataExt};
        for remaining in [30 * DAY, -DAY] {
            let temp = crate::private_tempdir();
            let first = prepare(temp.path()).unwrap();
            let original = snapshot(temp.path());
            let old = materials(&first);
            let latest_expiry = old
                .iter()
                .map(|tls| timestamp(tls.leaf.not_after()).unwrap())
                .max()
                .unwrap();
            let now = latest_expiry - remaining;
            let renewed = ServerIdentity::renew_at(temp.path(), now).unwrap();
            assert_eq!(first.request(), renewed.request());
            assert_eq!(first.server_info(), renewed.server_info());
            assert_immutable_files(&original, temp.path());
            let names = certificate_names(&first.prepared.config, first.request()).unwrap();
            for (paths, gossip) in [(renewed.tls_paths(), false), (renewed.gossip_paths(), true)] {
                let leaf = X509::from_pem(&fs::read(&paths.certificate).unwrap()).unwrap();
                assert_eq!(timestamp(leaf.not_before()).unwrap(), now);
                assert_eq!(timestamp(leaf.not_after()).unwrap(), now + LEAF_LIFETIME);
                assert_ne!(
                    &fs::read(&paths.certificate).unwrap(),
                    &original[paths.certificate.file_name().unwrap()]
                );
                for purpose in ["sslserver", "sslclient"] {
                    let result = std::process::Command::new("openssl")
                        .args([
                            "verify",
                            "-x509_strict",
                            "-attime",
                            &now.to_string(),
                            "-CAfile",
                        ])
                        .arg(&paths.ca_certificate)
                        .args(["-purpose", purpose, "-verify_hostname", &names.hostname])
                        .arg(&paths.certificate)
                        .output()
                        .unwrap();
                    assert_eq!(
                        result.status.success(),
                        gossip || purpose == "sslserver",
                        "{}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                }
                let meta = fs::metadata(&paths.certificate).unwrap();
                assert_eq!(meta.mode() & 0o7777, 0o600);
                assert_eq!(meta.nlink(), 1);
                assert_eq!(meta.uid(), rustix::process::geteuid().as_raw());
            }
            let before = snapshot(temp.path());
            let meta = fs::metadata(&renewed.tls_paths().fullchain).unwrap();
            ServerIdentity::renew_at(temp.path(), now).unwrap();
            assert_eq!(snapshot(temp.path()), before);
            let again = fs::metadata(&renewed.tls_paths().fullchain).unwrap();
            assert_eq!(meta.ino(), again.ino());
            assert_eq!(meta.modified().unwrap(), again.modified().unwrap());
        }
    }

    #[test]
    fn renewal_is_read_only_before_threshold_and_never_creates_identity() {
        let temp = crate::private_tempdir();
        let missing = temp.path().join("missing");
        assert!(ServerIdentity::renew(&missing).is_err());
        assert!(!missing.exists());
        assert!(ServerIdentity::renew(temp.path()).is_err());
        assert!(snapshot(temp.path()).is_empty());
        let identity = prepare(temp.path()).unwrap();
        let before = snapshot(temp.path());
        let renewed = ServerIdentity::renew(temp.path()).unwrap();
        assert_eq!(renewed.server_info(), identity.server_info());
        let expires = materials(&identity)
            .iter()
            .map(|m| timestamp(m.leaf.not_after()).unwrap())
            .min()
            .unwrap();
        ServerIdentity::renew_at(temp.path(), expires - RENEW_BEFORE - 1).unwrap();
        assert_eq!(snapshot(temp.path()), before);
        ServerIdentity::load(temp.path()).unwrap();
    }

    #[test]
    fn renewal_caps_leaf_validity_and_refuses_expired_or_future_ca() {
        let temp = crate::private_tempdir();
        let identity = prepare(temp.path()).unwrap();
        let tls = materials(&identity);
        let ca_start = tls
            .iter()
            .map(|m| timestamp(m.ca.not_before()).unwrap())
            .max()
            .unwrap();
        let ca_end = tls
            .iter()
            .map(|m| timestamp(m.ca.not_after()).unwrap())
            .min()
            .unwrap();
        for now in [ca_start - 1, ca_end] {
            let before = snapshot(temp.path());
            assert!(ServerIdentity::renew_at(temp.path(), now).is_err());
            assert_eq!(snapshot(temp.path()), before);
        }
        ServerIdentity::renew_at(temp.path(), ca_end - 10 * DAY).unwrap();
        for paths in [identity.tls_paths(), identity.gossip_paths()] {
            let leaf = X509::from_pem(&std::fs::read(&paths.certificate).unwrap()).unwrap();
            let ca = X509::from_pem(&std::fs::read(&paths.ca_certificate).unwrap()).unwrap();
            assert_eq!(leaf.not_after(), ca.not_after());
        }
        let before = snapshot(temp.path());
        ServerIdentity::renew_at(temp.path(), ca_end - 9 * DAY).unwrap();
        assert_eq!(snapshot(temp.path()), before);
    }

    #[test]
    fn renewal_refuses_corrupt_bindings_and_future_leaf_before_any_writes() {
        let temp = crate::private_tempdir();
        let identity = prepare(temp.path()).unwrap();
        let original = snapshot(temp.path());
        let [tls, _] = materials(&identity);
        let names = certificate_names(&identity.prepared.config, identity.request()).unwrap();
        let now = current_time().unwrap();
        let wrong_key = new_key().unwrap();
        for target in [
            &identity.tls_paths().ca_private_key,
            &identity.tls_paths().private_key,
        ] {
            std::fs::write(target, wrong_key.private_key_to_pem_pkcs8().unwrap()).unwrap();
            let before = snapshot(temp.path());
            assert!(ServerIdentity::renew_at(temp.path(), now + LEAF_LIFETIME + DAY).is_err());
            assert_eq!(snapshot(temp.path()), before);
            std::fs::write(target, &original[target.file_name().unwrap()]).unwrap();
        }
        for leaf in [
            issue_leaf(
                &tls.ca,
                &wrong_key,
                &tls.leaf_key,
                &names,
                false,
                now,
                now + DAY,
            )
            .unwrap(),
            issue_leaf(
                &tls.ca,
                &tls.ca_key,
                &tls.leaf_key,
                &names,
                false,
                now + DAY,
                now + LEAF_LIFETIME,
            )
            .unwrap(),
        ] {
            let pem = leaf.to_pem().unwrap();
            std::fs::write(&identity.tls_paths().certificate, &pem).unwrap();
            std::fs::write(
                &identity.tls_paths().fullchain,
                [pem, tls.ca.to_pem().unwrap()].concat(),
            )
            .unwrap();
            let before = snapshot(temp.path());
            assert!(ServerIdentity::renew_at(temp.path(), now).is_err());
            assert_eq!(snapshot(temp.path()), before);
        }
    }
    fn restore_snapshot(
        path: &Path,
        files: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
    ) {
        let dir = secure::Directory::open(path, false).unwrap();
        for (name, bytes) in files {
            dir.write_new(name.to_str().unwrap(), bytes).unwrap();
        }
    }

    fn interrupt_renewal(path: &Path, now: i64, boundary: usize) -> Result<ServerIdentity> {
        RENEWAL_FAILURE.with(|failure| failure.set(Some(boundary)));
        let result = ServerIdentity::renew_at(path, now);
        RENEWAL_FAILURE.with(|failure| failure.set(None));
        result
    }

    fn assert_recovered(
        path: &Path,
        now: i64,
        original: &ServerIdentity,
        before: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
    ) {
        let renewed = ServerIdentity::renew_at(path, now).unwrap();
        assert_eq!(renewed.request(), original.request());
        assert_eq!(renewed.server_info(), original.server_info());
        assert_immutable_files(before, path);
        assert!(!path.join(RENEWAL_JOURNAL).exists());
        assert!(!path.join(RENEWAL_STAGE).exists());
        let after = snapshot(path);
        ServerIdentity::renew_at(path, now).unwrap();
        assert_eq!(snapshot(path), after);
    }

    #[test]
    fn renewal_recovers_at_every_write_and_sync_boundary_including_recovery_itself() {
        let seed = crate::private_tempdir();
        let original = prepare(seed.path()).unwrap();
        let before = snapshot(seed.path());
        let now = timestamp(materials(&original)[0].leaf.not_after()).unwrap() + DAY;
        let mut interrupted = Vec::new();
        let mut failures = 0;
        for boundary in 0..64 {
            let case = crate::private_tempdir();
            restore_snapshot(case.path(), &before);
            match interrupt_renewal(case.path(), now, boundary) {
                Ok(_) => break,
                Err(error) => assert!(
                    error.to_string().contains("injected renewal interruption"),
                    "{error}"
                ),
            }
            failures += 1;
            if case.path().join(RENEWAL_JOURNAL).exists() {
                let data = std::fs::read_to_string(case.path().join(RENEWAL_JOURNAL)).unwrap();
                assert!(!data.contains("PRIVATE"));
                assert!(
                    ServerIdentity::load(case.path())
                        .unwrap_err()
                        .to_string()
                        .contains("pending leaf renewal")
                );
                if boundary == 8 {
                    // Leaf rename is visible, while its fullchain still contains the old leaf.
                    assert_ne!(
                        std::fs::read(case.path().join("https-leaf.pem")).unwrap(),
                        before[std::ffi::OsStr::new("https-leaf.pem")]
                    );
                    assert_eq!(
                        std::fs::read(case.path().join("https-fullchain.pem")).unwrap(),
                        before[std::ffi::OsStr::new("https-fullchain.pem")]
                    );
                }
                if boundary == 7 || boundary == 8 {
                    interrupted.push((snapshot(case.path()), if boundary == 7 { 24 } else { 17 }));
                }
            }
            assert_recovered(case.path(), now, &original, &before);
        }
        assert_eq!(failures, 27); // journal + four certificate replacements + journal removal
        assert_eq!(interrupted.len(), 2);
        for (interrupted, expected_failures) in interrupted {
            let mut failures = 0;
            for boundary in 0..64 {
                let case = crate::private_tempdir();
                restore_snapshot(case.path(), &interrupted);
                if interrupt_renewal(case.path(), now, boundary).is_ok() {
                    break;
                }
                failures += 1;
                assert_recovered(case.path(), now, &original, &before);
            }
            assert_eq!(failures, expected_failures);
        }
    }

    #[test]
    fn recovery_discards_only_safe_uncommitted_staging_data() {
        let seed = crate::private_tempdir();
        let original = prepare(seed.path()).unwrap();
        let before = snapshot(seed.path());
        let now = timestamp(materials(&original)[0].leaf.not_after()).unwrap() + DAY;
        for committed in [false, true] {
            for fragment in [
                b"".as_slice(),
                b"{\"version\":",
                b"-----BEGIN CERTIFICATE-----\npartial",
            ] {
                let case = crate::private_tempdir();
                restore_snapshot(case.path(), &before);
                if committed {
                    assert!(interrupt_renewal(case.path(), now, 5).is_err());
                }
                let dir = secure::Directory::open(case.path(), false).unwrap();
                if case.path().join(RENEWAL_STAGE).exists() {
                    std::fs::write(case.path().join(RENEWAL_STAGE), fragment).unwrap();
                } else {
                    dir.write_new(RENEWAL_STAGE, fragment).unwrap();
                }
                assert_recovered(case.path(), now, &original, &before);
            }
        }
    }

    #[test]
    fn recovery_rejects_journal_tampering_and_unrelated_disk_state_without_writes() {
        let seed = crate::private_tempdir();
        let identity = prepare(seed.path()).unwrap();
        let now = timestamp(materials(&identity)[0].leaf.not_after()).unwrap() + DAY;
        assert!(interrupt_renewal(seed.path(), now, 4).is_err());
        let baseline = snapshot(seed.path());
        let journal: serde_json::Value =
            serde_json::from_slice(&baseline[std::ffi::OsStr::new(RENEWAL_JOURNAL)]).unwrap();
        for change in 0..9 {
            let case = crate::private_tempdir();
            restore_snapshot(case.path(), &baseline);
            let mut corrupted = journal.clone();
            match change {
                0 => corrupted["version"] = 2.into(),
                1 => {
                    corrupted["prepared_digest"] = KeyFingerprint::of_bytes(b"other identity")
                        .to_string()
                        .into()
                }
                2 => {
                    corrupted["https"]["replacement_pem"] =
                        journal["gossip"]["replacement_pem"].clone()
                }
                3 => corrupted["gossip"]["previous_pem"] = "bad PEM".into(),
                4 => corrupted["https"]["path"] = "../victim".into(),
                5 => std::fs::write(case.path().join("https-leaf.pem"), b"corrupt leaf").unwrap(),
                6 => std::fs::write(case.path().join("gossip-fullchain.pem"), b"corrupt chain")
                    .unwrap(),
                7 => std::fs::write(case.path().join(RENEWAL_JOURNAL), b"{").unwrap(),
                8 => std::fs::write(
                    case.path().join(RENEWAL_JOURNAL),
                    vec![b' '; MAX_FILE_BYTES + 1],
                )
                .unwrap(),
                _ => unreachable!(),
            }
            if change < 5 {
                std::fs::write(
                    case.path().join(RENEWAL_JOURNAL),
                    serde_json::to_vec(&corrupted).unwrap(),
                )
                .unwrap();
            }
            let before = snapshot(case.path());
            assert!(
                ServerIdentity::renew_at(case.path(), now).is_err(),
                "case {change}"
            );
            assert_eq!(snapshot(case.path()), before);
        }
        // Even a valid journal cannot waive CA expiry, or historical validation
        // of a replacement that itself expired while the server was offline.
        let ca_end = timestamp(
            X509::from_pem(&baseline[std::ffi::OsStr::new("https-ca.pem")])
                .unwrap()
                .not_after(),
        )
        .unwrap();
        assert!(ServerIdentity::renew_at(seed.path(), ca_end).is_err());
        assert_eq!(snapshot(seed.path()), baseline);
        assert_recovered(seed.path(), now + 400 * DAY, &identity, &baseline);
    }

    #[test]
    fn journal_and_stage_reject_symlinks_hardlinks_and_unsafe_permissions() {
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
        };
        let seed = crate::private_tempdir();
        let identity = prepare(seed.path()).unwrap();
        let now = timestamp(materials(&identity)[0].leaf.not_after()).unwrap() + DAY;
        assert!(interrupt_renewal(seed.path(), now, 5).is_err());
        let baseline = snapshot(seed.path());
        for name in [RENEWAL_JOURNAL, RENEWAL_STAGE] {
            for change in 0..4 {
                let case = crate::private_tempdir();
                let path = case.path().join("identity");
                std::fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                restore_snapshot(&path, &baseline);
                let target = path.join(name);
                let victim = case.path().join("victim");
                match change {
                    0 => fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap(),
                    1 => fs::hard_link(&target, &victim).unwrap(),
                    2 => {
                        fs::rename(&target, &victim).unwrap();
                        symlink(&victim, &target).unwrap();
                    }
                    3 => {
                        fs::remove_file(&target).unwrap();
                        fs::create_dir(&target).unwrap();
                    }
                    _ => unreachable!(),
                }
                assert!(ServerIdentity::renew_at(&path, now).is_err());
                assert_immutable_files(&baseline, &path);
                assert_eq!(
                    fs::read(path.join("https-leaf.pem")).unwrap(),
                    baseline[std::ffi::OsStr::new("https-leaf.pem")]
                );
                if change == 2 {
                    assert_eq!(
                        fs::read(&victim).unwrap(),
                        baseline[std::ffi::OsStr::new(name)]
                    );
                }
            }
        }
    }

    #[test]
    fn concurrent_renewal_uses_exclusive_inode_lock_and_one_leaf_generation() {
        let temp = crate::private_tempdir();
        let identity = prepare(temp.path()).unwrap();
        let before = snapshot(temp.path());
        let now = timestamp(materials(&identity)[0].leaf.not_after()).unwrap() + DAY;
        let dir = secure::Directory::open(temp.path(), false).unwrap();
        dir.lock().unwrap();
        let other = rustix::fs::open(
            temp.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        assert_eq!(
            rustix::fs::flock(&other, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .unwrap_err(),
            rustix::io::Errno::WOULDBLOCK
        );
        drop(dir);
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        ServerIdentity::renew_at(temp.path(), now).unwrap()
                    })
                })
                .collect();
            for worker in workers {
                let result = worker.join().unwrap();
                assert_eq!(result.server_info(), identity.server_info());
            }
        });
        assert_recovered(temp.path(), now, &identity, &before);
    }
    #[test]
    fn renewal_refuses_missing_or_wrong_eku_ca_leaf_and_mismatched_sans() {
        let temp = crate::private_tempdir();
        let identity = prepare(temp.path()).unwrap();
        let tls = materials(&identity);
        let names = certificate_names(&identity.prepared.config, identity.request()).unwrap();
        for (material, paths, gossip) in [
            (&tls[0], identity.tls_paths(), false),
            (&tls[1], identity.gossip_paths(), true),
        ] {
            let original = snapshot(temp.path());
            for corruption in 0..4 {
                let mut leaf =
                    certificate(&material.leaf_key, "malformed managed leaf", 365).unwrap();
                leaf.set_not_before(material.ca.not_before()).unwrap();
                let end = Asn1Time::from_unix(current_time().unwrap() + DAY).unwrap();
                leaf.set_not_after(&end).unwrap();
                leaf.set_issuer_name(material.ca.subject_name()).unwrap();
                let mut constraints = BasicConstraints::new();
                constraints.critical();
                if corruption == 2 {
                    constraints.ca();
                }
                leaf.append_extension(constraints.build().unwrap()).unwrap();
                leaf.append_extension(
                    KeyUsage::new()
                        .critical()
                        .digital_signature()
                        .build()
                        .unwrap(),
                )
                .unwrap();
                if corruption != 0 {
                    let mut eku = ExtendedKeyUsage::new();
                    if corruption == 1 {
                        if gossip {
                            eku.server_auth();
                        } else {
                            eku.client_auth();
                        }
                    } else {
                        eku.server_auth();
                        if gossip {
                            eku.client_auth();
                        }
                    }
                    leaf.append_extension(eku.build().unwrap()).unwrap();
                }
                let mut san = SubjectAlternativeName::new();
                san.dns(if corruption == 3 {
                    "other.tako.internal"
                } else {
                    &names.hostname
                });
                if gossip {
                    san.dns(&names.gossip);
                }
                for ip in names.ips {
                    san.ip(&ip.to_string());
                }
                let san = san
                    .build(&leaf.x509v3_context(Some(&material.ca), None))
                    .unwrap();
                leaf.append_extension(san).unwrap();
                let aki = AuthorityKeyIdentifier::new()
                    .keyid(true)
                    .build(&leaf.x509v3_context(Some(&material.ca), None))
                    .unwrap();
                leaf.append_extension(aki).unwrap();
                leaf.sign(&material.ca_key, MessageDigest::sha256())
                    .unwrap();
                let pem = leaf.build().to_pem().unwrap();
                std::fs::write(&paths.certificate, &pem).unwrap();
                std::fs::write(
                    &paths.fullchain,
                    [pem, material.ca.to_pem().unwrap()].concat(),
                )
                .unwrap();
                let before = snapshot(temp.path());
                assert!(
                    ServerIdentity::renew(temp.path()).is_err(),
                    "role {gossip}, corruption {corruption}"
                );
                assert_eq!(snapshot(temp.path()), before);
            }
            for path in [&paths.certificate, &paths.fullchain] {
                std::fs::write(path, &original[path.file_name().unwrap()]).unwrap();
            }
        }
    }
}
