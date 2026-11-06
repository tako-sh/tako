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
mod secure;
#[cfg(not(unix))]
#[path = "server_identity/unsupported.rs"]
mod secure;

#[cfg(all(test, unix))]
mod tests;
