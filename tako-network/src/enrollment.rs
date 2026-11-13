use crate::{
    DeviceId, DeviceKind, DeviceName, EncryptionPublicKey, NetworkId, RequestId, Result,
    SshPublicKey, WgPublicKey, error::invalid,
};
use serde::{Deserialize, Serialize};

pub const ENROLLMENT_VERSION: u32 = 2;
pub const ENROLLMENT_NAMESPACE: &str = "tako-network-enrollment-v2";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentRequest {
    pub version: u32,
    pub network_id: Option<NetworkId>,
    pub kind: DeviceKind,
    pub request_id: RequestId,
    pub device_id: Option<DeviceId>,
    pub name: DeviceName,
    pub wg_public_key: WgPublicKey,
    pub ssh_public_key: SshPublicKey,
    pub encryption_public_key: EncryptionPublicKey,
    pub expires_at: Option<u64>,
    pub proof: String,
}

impl EnrollmentRequest {
    pub fn verify(&self) -> Result<()> {
        if self.version != ENROLLMENT_VERSION {
            return Err(invalid("enrollment version", "unsupported version"));
        }
        if self.encryption_public_key.as_bytes() == self.wg_public_key.as_bytes() {
            return Err(invalid(
                "encryption public key",
                "must be independent of WireGuard",
            ));
        }
        match (&self.network_id, &self.device_id) {
            (Some(network), Some(id)) if *id == self.device_id_for(network)? => {}
            (None, None) => {}
            _ => {
                return Err(invalid(
                    "device ID",
                    "does not match target network, kind, and keys",
                ));
            }
        }
        if self.proof.len() > 2048 {
            return Err(invalid("enrollment proof", "signature too large"));
        }
        let signature = ssh_key::SshSig::from_pem(&self.proof)?;
        if signature.hash_alg() != ssh_key::HashAlg::Sha256
            || signature.to_pem(ssh_key::LineEnding::LF)? != self.proof
        {
            return Err(invalid(
                "enrollment proof",
                "expected canonical SHA-256 SSHSIG",
            ));
        }
        self.ssh_public_key.parsed()?.verify(
            ENROLLMENT_NAMESPACE,
            &self.canonical_bytes()?,
            &signature,
        )?;
        Ok(())
    }

    pub fn device_id_for(&self, network: &NetworkId) -> Result<DeviceId> {
        if self
            .network_id
            .as_ref()
            .is_some_and(|target| target != network)
        {
            return Err(invalid("network ID", "request targets a different network"));
        }
        DeviceId::from_keys(
            network,
            self.kind,
            &self.ssh_public_key,
            &self.wg_public_key,
        )
    }

    /// Fixed framing: domain, version, IDs, length-prefixed name, WG bytes,
    /// length-prefixed SSH wire key and encryption key. Integers are big-endian.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = b"tako-network-enrollment\0".to_vec();
        bytes.extend_from_slice(&self.version.to_be_bytes());
        match &self.network_id {
            Some(network) => {
                bytes.push(1);
                bytes.extend_from_slice(network.as_bytes());
            }
            None => bytes.push(0),
        }
        bytes.push(self.kind.tag());
        bytes.extend_from_slice(self.request_id.as_bytes());
        match &self.device_id {
            Some(id) => {
                bytes.push(1);
                bytes.extend_from_slice(id.as_bytes());
            }
            None => bytes.push(0),
        }
        match self.expires_at {
            Some(expiry) => {
                bytes.push(1);
                bytes.extend_from_slice(&expiry.to_be_bytes());
            }
            None => bytes.push(0),
        }
        append_field(&mut bytes, self.name.as_str().as_bytes());
        bytes.extend_from_slice(self.wg_public_key.as_bytes());
        append_field(&mut bytes, &self.ssh_public_key.parsed()?.to_bytes()?);
        bytes.extend_from_slice(self.encryption_public_key.as_bytes());
        Ok(bytes)
    }
}

fn append_field(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
    bytes.extend_from_slice(field);
}
