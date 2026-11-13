//! Application calls carry an agent-signed proof, separate from ordinary private HTTP.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    time::{Duration, Instant},
};
use tako_core::network::NetworkInvocation;
use tako_network::{
    AppEnv, DeviceId, DeviceKind, LocalDeviceKeys, NetworkId, SshPublicKey,
    services::CLOCK_SKEW_SECONDS,
};

const NAMESPACE: &str = "tako-network-invocation-v1";
const LIFETIME: u64 = 30;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignedInvocation {
    pub network: NetworkId,
    pub server: DeviceId,
    pub destination: DeviceId,
    pub caller: AppEnv,
    pub target: AppEnv,
    pub issued_at: u64,
    pub nonce: String,
    pub invocation: NetworkInvocation,
    pub signature: String,
}
impl SignedInvocation {
    pub fn sign(
        keys: &LocalDeviceKeys,
        network: NetworkId,
        destination: DeviceId,
        caller: AppEnv,
        target: AppEnv,
        invocation: NetworkInvocation,
        now: u64,
    ) -> io::Result<Self> {
        let server = keys
            .device_id(&network, DeviceKind::Server)
            .map_err(io::Error::other)?;
        let mut call = Self {
            network,
            server,
            destination,
            caller,
            target,
            issued_at: now,
            nonce: nanoid::nanoid!(32),
            invocation,
            signature: String::new(),
        };
        call.validate(now)?;
        call.signature = keys
            .sign(NAMESPACE, &call.transcript()?)
            .map_err(io::Error::other)?
            .to_pem(ssh_key::LineEnding::LF)
            .map_err(io::Error::other)?;
        Ok(call)
    }
    fn transcript(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(&(
            NAMESPACE,
            &self.network,
            &self.server,
            &self.destination,
            &self.caller,
            &self.target,
            self.issued_at,
            &self.nonce,
            &self.invocation,
        ))
        .map_err(io::Error::other)
    }
    fn validate(&self, now: u64) -> io::Result<()> {
        if self.issued_at > now.saturating_add(CLOCK_SKEW_SECONDS)
            || now > self.issued_at.saturating_add(LIFETIME)
            || self.signature.len() > 2048
            || self.nonce.len() != 32
            || !self
                .nonce
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            || self.caller.as_str().split_once('/').map(|(_, env)| env)
                != self.target.as_str().split_once('/').map(|(_, env)| env)
            || !tako_core::network::valid_primitive_name(&self.invocation.name)
        {
            return Err(io::Error::other("invalid network invocation proof"));
        }
        Ok(())
    }
    pub fn verify(&self, key: &SshPublicKey, now: u64) -> io::Result<()> {
        self.validate(now)?;
        let key = ssh_key::PublicKey::from_openssh(key.as_str()).map_err(io::Error::other)?;
        let signature = ssh_key::SshSig::from_pem(&self.signature).map_err(io::Error::other)?;
        key.verify(NAMESPACE, &self.transcript()?, &signature)
            .map_err(io::Error::other)
    }
}

#[derive(Default)]
pub(crate) struct ReplayGuard(BTreeMap<(DeviceId, String), Instant>);
impl ReplayGuard {
    /// Capacity exhaustion fails closed; an accepted invocation is never evicted early.
    pub fn accept(&mut self, call: &SignedInvocation) -> bool {
        self.accept_at(call, Instant::now())
    }
    fn accept_at(&mut self, call: &SignedInvocation, now: Instant) -> bool {
        self.0.retain(|_, expiry| *expiry > now);
        if self.0.len() >= 8192 {
            return false;
        }
        let key = (call.server.clone(), call.nonce.clone());
        if self.0.contains_key(&key) {
            return false;
        }
        // Proof timestamps have whole-second precision and accept the entire
        // last second, including when the signer is five seconds ahead.
        self.0.insert(
            key,
            now + Duration::from_secs(LIFETIME + CLOCK_SKEW_SECONDS + 1),
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_guard_covers_clock_skew_and_the_last_accepted_second() {
        let keys = LocalDeviceKeys::generate().unwrap();
        let network = NetworkId::random().unwrap();
        let server = keys.device_id(&network, DeviceKind::Server).unwrap();
        let call = SignedInvocation::sign(
            &keys,
            network,
            server,
            "caller/prod".parse().unwrap(),
            "target/prod".parse().unwrap(),
            NetworkInvocation {
                name: "lookup".into(),
                request: tako_core::network::NetworkRequest::Call {
                    input: serde_json::Value::Null,
                },
            },
            105,
        )
        .unwrap();
        let mut replay = ReplayGuard::default();
        let start = Instant::now();
        assert!(call.validate(100).is_ok());
        assert!(replay.accept_at(&call, start));
        assert!(call.validate(135).is_ok());
        assert!(!replay.accept_at(&call, start + Duration::from_millis(35_999)));
        assert!(call.validate(136).is_err());
    }
}
