use std::{collections::BTreeSet, net::Ipv6Addr};

use serde::{Deserialize, Serialize};

use crate::{
    AppEnv, DeviceId, DeviceKind, LocalDeviceKeys, Member, MemberKind, NetworkId, RegistrySnapshot,
    Result, error::invalid,
};

pub const SERVICE_NAMESPACE: &str = "tako-native-services-v1";
pub const MAX_LEASE_SECONDS: u64 = 30;
pub const RENEW_SECONDS: u64 = 10;
/// How far ahead of a verifier's clock a signed timestamp may be. Servers on
/// one network keep time independently, so a fresh signature can look early.
pub const CLOCK_SKEW_SECONDS: u64 = 5;
pub const MAX_SERVICES: usize = 256;
pub const MAX_LEASE_BYTES: usize = 64 * 1024;
pub const SERVICE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tako_service_leases (server_id TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL DEFAULT '');";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceLease {
    pub network_id: NetworkId,
    pub server_id: DeviceId,
    pub issued_at: u64,
    pub expires_at: u64,
    pub apps: Vec<AppEnv>,
    pub signature: String,
}

impl ServiceLease {
    pub fn sign(
        keys: &LocalDeviceKeys,
        network_id: NetworkId,
        apps: BTreeSet<AppEnv>,
        now: u64,
    ) -> Result<Self> {
        let mut lease = Self {
            server_id: keys.device_id(&network_id, DeviceKind::Server)?,
            network_id,
            issued_at: now,
            expires_at: now
                .checked_add(MAX_LEASE_SECONDS)
                .ok_or_else(|| invalid("service lease", "clock overflow"))?,
            apps: apps.into_iter().collect(),
            signature: String::new(),
        };
        lease.validate()?;
        lease.signature = keys
            .sign(SERVICE_NAMESPACE, &lease.transcript()?)?
            .to_pem(ssh_key::LineEnding::LF)?;
        Ok(lease)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_LEASE_BYTES {
            return Err(invalid("service lease", "too large"));
        }
        let lease: Self = serde_json::from_slice(bytes)?;
        lease.validate()?;
        Ok(lease)
    }

    fn validate(&self) -> Result<()> {
        if self.apps.len() > MAX_SERVICES
            || self.apps.windows(2).any(|pair| pair[0] >= pair[1])
            || self.signature.len() > 2048
            || self.expires_at <= self.issued_at
            || self.expires_at - self.issued_at > MAX_LEASE_SECONDS
        {
            return Err(invalid(
                "service lease",
                "invalid lifetime, ordering, or size",
            ));
        }
        Ok(())
    }

    fn transcript(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            SERVICE_NAMESPACE,
            &self.network_id,
            &self.server_id,
            self.issued_at,
            self.expires_at,
            &self.apps,
        ))?)
    }

    pub fn verify(&self, registry: &RegistrySnapshot, now: u64) -> Result<Ipv6Addr> {
        let effective = registry.effective_members(now)?;
        let member = effective
            .get(&self.server_id)
            .ok_or_else(|| crate::Error::NotEffective(self.server_id.clone()))?;
        self.verify_member(&registry.network.id, member, now)
    }

    /// `member` must come from the current enforced effective-members projection.
    ///
    /// `issued_at` may lead `now` by `CLOCK_SKEW_SECONDS`, or a publisher whose
    /// clock runs ahead would lose its placements after every renewal. Expiry
    /// stays strict, so the result always satisfies `expires_at > now`.
    pub fn verify_member(
        &self,
        network: &NetworkId,
        member: &Member,
        now: u64,
    ) -> Result<Ipv6Addr> {
        self.validate()?;
        if &self.network_id != network
            || member.network_id != *network
            || member.device_id != self.server_id
            || !matches!(member.kind, MemberKind::Server(_))
            || !member.permissions.is_full()
            || self.issued_at > now.saturating_add(CLOCK_SKEW_SECONDS)
            || self.expires_at <= now
        {
            return Err(invalid(
                "service lease",
                "wrong network, publisher, or time",
            ));
        }
        let signature = ssh_key::SshSig::from_pem(&self.signature)?;
        if signature.hash_alg() != ssh_key::HashAlg::Sha256
            || signature.to_pem(ssh_key::LineEnding::LF)? != self.signature
        {
            return Err(invalid("service lease", "noncanonical signature"));
        }
        member.request.ssh_public_key.parsed()?.verify(
            SERVICE_NAMESPACE,
            &self.transcript()?,
            &signature,
        )?;
        Ok(network.address(&member.request.wg_public_key))
    }
}

/// Private network names live under this single-label suffix.
pub const PRIVATE_DOMAIN: &str = "tako";
/// Internal server identity names. Never resolvable and never public routes.
const IDENTITY_DOMAIN: &str = "tako.internal";

pub fn service_hostname(app: &AppEnv) -> String {
    let (app, env) = app.as_str().split_once('/').expect("validated AppEnv");
    format!("{app}.{env}.{PRIVATE_DOMAIN}")
}

/// Lowercased DNS label for a server name, when the name is usable as one.
pub fn server_label(name: &str) -> Option<String> {
    let label = name.to_ascii_lowercase();
    let bytes = label.as_bytes();
    (!bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_'))
    .then_some(label)
}

pub fn server_hostname(name: &str) -> Option<String> {
    server_label(name).map(|label| format!("{label}.{PRIVATE_DOMAIN}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceHost {
    Public,
    Reserved,
    App(AppEnv),
    Server(String),
}

fn in_domain(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Classify the raw HTTP authority or DNS name before public wildcard route matching.
pub fn classify_host(authority: &str) -> ServiceHost {
    let host = authority
        .split_once(':')
        .map_or(authority, |(host, _)| host);
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if in_domain(&host, IDENTITY_DOMAIN) {
        return ServiceHost::Reserved;
    }
    if !in_domain(&host, PRIVATE_DOMAIN) {
        return ServiceHost::Public;
    }
    if authority.len() > 254
        || authority.bytes().any(|b| b.is_ascii_whitespace())
        || authority.split_once(':').is_some_and(|(_, port)| {
            port.is_empty()
                || !port.bytes().all(|b| b.is_ascii_digit())
                || port.parse::<u16>().is_err()
        })
    {
        return ServiceHost::Reserved;
    }
    let Some(labels) = host.strip_suffix(&format!(".{PRIVATE_DOMAIN}")) else {
        return ServiceHost::Reserved;
    };
    match labels.split('.').collect::<Vec<_>>().as_slice() {
        [server] => server_label(server).map_or(ServiceHost::Reserved, ServiceHost::Server),
        [app, env] => match format!("{app}/{env}").parse() {
            Ok(scope) => ServiceHost::App(scope),
            Err(_) => ServiceHost::Reserved,
        },
        _ => ServiceHost::Reserved,
    }
}

#[cfg(test)]
mod tests;
