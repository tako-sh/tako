//! Bounded, signed native RPC messages shared by clients and server transports.
use serde::{Deserialize, Serialize};

use crate::{
    DeviceId, DeviceKind, EnrollmentRequest, LocalDeviceKeys, MemberKind, NetworkId,
    NetworkPublicProfile, PendingApprovalProfile, Permissions, RegistrySnapshot, RequestId,
    Revocation, RevocationId, SshPublicKey, WgChallenge, WgProof,
};

pub const RPC_NAMESPACE: &str = "tako-network-rpc-v1";
pub const MAX_RPC_BYTES: usize = 128 * 1024;
pub const MAX_SIGNATURE_BYTES: usize = 2048;
pub type RuntimeResult<T> = Result<T, RuntimeError>;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Domain(#[from] crate::Error),
    #[error("invalid native RPC: {0}")]
    Invalid(&'static str),
    #[error("native RPC authentication failed")]
    Authentication,
    #[error("native RPC permission denied")]
    Denied,
    #[error("native RPC nonce already consumed")]
    Replay,
    #[error("native runtime capacity exceeded: {0}")]
    Capacity(&'static str),
    #[error("conflicting immutable record: {0}")]
    Conflict(&'static str),
    #[error("pending enrollment unavailable or expired")]
    PendingUnavailable,
    #[error("WireGuard challenge unavailable or expired")]
    ChallengeUnavailable,
    #[error("Corrosion HTTP request failed")]
    Transport,
    #[error("invalid Corrosion response: {0}")]
    Corrosion(&'static str),
    #[error("local runtime database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("runtime I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid runtime JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveRequest {
    pub pending_id: RequestId,
    pub request: EnrollmentRequest,
    pub kind: MemberKind,
    pub permissions: Permissions,
    pub expires_at: u64,
    pub member_expires_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RpcRequest {
    Approve(Box<ApproveRequest>),
    Begin {
        pending_id: RequestId,
    },
    Complete {
        pending_id: RequestId,
        challenge_id: RequestId,
        proof: WgProof,
    },
    Snapshot,
    Revoke {
        id: RevocationId,
        target: DeviceId,
    },
}

impl RpcRequest {
    pub fn path(&self) -> &'static str {
        match self {
            Self::Approve(_) => "/v1/network/approve",
            Self::Begin { .. } => "/v1/network/begin",
            Self::Complete { .. } => "/v1/network/complete",
            Self::Snapshot => "/v1/network/snapshot",
            Self::Revoke { .. } => "/v1/network/revoke",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequest {
    pub network_id: NetworkId,
    pub server_id: DeviceId,
    pub signer: DeviceId,
    pub timestamp: u64,
    pub nonce: RequestId,
    pub request: RpcRequest,
    pub signature: String,
}

impl SignedRequest {
    pub fn sign(
        keys: &LocalDeviceKeys,
        network_id: NetworkId,
        server_id: DeviceId,
        signer_kind: DeviceKind,
        timestamp: u64,
        request: RpcRequest,
    ) -> RuntimeResult<Self> {
        let mut signed = Self {
            signer: keys.device_id(&network_id, signer_kind)?,
            network_id,
            server_id,
            timestamp,
            nonce: RequestId::random()?,
            request,
            signature: String::new(),
        };
        signed.signature = keys
            .sign(RPC_NAMESPACE, &signed.transcript()?)?
            .to_pem(ssh_key::LineEnding::LF)
            .map_err(crate::Error::from)?;
        signed.check_size()?;
        Ok(signed)
    }

    pub fn from_slice(bytes: &[u8]) -> RuntimeResult<Self> {
        if bytes.len() > MAX_RPC_BYTES {
            return Err(RuntimeError::Capacity("RPC bytes"));
        }
        let request: Self = serde_json::from_slice(bytes)?;
        request.check_size()?;
        Ok(request)
    }

    pub(crate) fn check_size(&self) -> RuntimeResult<()> {
        if self.signature.len() > MAX_SIGNATURE_BYTES
            || serde_json::to_vec(self)?.len() > MAX_RPC_BYTES
        {
            return Err(RuntimeError::Capacity("RPC bytes"));
        }
        Ok(())
    }

    fn transcript(&self) -> RuntimeResult<Vec<u8>> {
        let body = serde_json::to_vec(&self.request)?;
        if body.len() > MAX_RPC_BYTES {
            return Err(RuntimeError::Capacity("RPC body"));
        }
        let mut bytes = b"tako-network-native-rpc-v1\0".to_vec();
        bytes.extend_from_slice(self.network_id.as_bytes());
        bytes.extend_from_slice(self.server_id.as_bytes());
        bytes.extend_from_slice(self.signer.as_bytes());
        for field in ["POST", self.request.path()] {
            bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bytes.extend_from_slice(field.as_bytes());
        }
        bytes.extend_from_slice(&self.timestamp.to_be_bytes());
        bytes.extend_from_slice(self.nonce.as_bytes());
        bytes.extend_from_slice(&openssl::sha::sha256(&body));
        Ok(bytes)
    }

    pub(crate) fn verify(&self, key: &SshPublicKey) -> RuntimeResult<()> {
        self.check_size()?;
        let signature =
            ssh_key::SshSig::from_pem(&self.signature).map_err(|_| RuntimeError::Authentication)?;
        if signature.hash_alg() != ssh_key::HashAlg::Sha256
            || signature
                .to_pem(ssh_key::LineEnding::LF)
                .map_err(|_| RuntimeError::Authentication)?
                != self.signature
        {
            return Err(RuntimeError::Authentication);
        }
        key.parsed()?
            .verify(RPC_NAMESPACE, &self.transcript()?, &signature)
            .map_err(|_| RuntimeError::Authentication)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RpcResponse {
    Pending(PendingApprovalProfile),
    Challenge {
        challenge_id: RequestId,
        challenge: WgChallenge,
    },
    Active(NetworkPublicProfile),
    Snapshot(RegistrySnapshot),
    Revoked(Revocation),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_bind_every_audience_and_body_field() {
        let keys = LocalDeviceKeys::generate().unwrap();
        let network = NetworkId::random().unwrap();
        let server = keys.device_id(&network, DeviceKind::Server).unwrap();
        let signed = SignedRequest::sign(
            &keys,
            network,
            server,
            DeviceKind::Laptop,
            123,
            RpcRequest::Snapshot,
        )
        .unwrap();
        let public = keys.ssh_public_key().unwrap();
        signed.verify(&public).unwrap();
        let roundtrip = SignedRequest::from_slice(&serde_json::to_vec(&signed).unwrap()).unwrap();
        assert_eq!(signed, roundtrip);
        for change in 0..7 {
            let mut tampered = signed.clone();
            match change {
                0 => tampered.network_id = NetworkId::random().unwrap(),
                1 => {
                    tampered.server_id = LocalDeviceKeys::generate()
                        .unwrap()
                        .device_id(&signed.network_id, DeviceKind::Server)
                        .unwrap()
                }
                2 => tampered.signer = signed.server_id.clone(),
                3 => tampered.timestamp += 1,
                4 => tampered.nonce = RequestId::random().unwrap(),
                5 => {
                    tampered.request = RpcRequest::Begin {
                        pending_id: RequestId::random().unwrap(),
                    }
                }
                _ => tampered.signature.push('\n'),
            }
            assert!(tampered.verify(&public).is_err(), "field {change}");
        }
        assert!(
            signed
                .verify(
                    &LocalDeviceKeys::generate()
                        .unwrap()
                        .ssh_public_key()
                        .unwrap()
                )
                .is_err()
        );
    }

    #[test]
    fn rejects_oversized_and_unknown_wire_fields() {
        assert!(SignedRequest::from_slice(&vec![b' '; MAX_RPC_BYTES + 1]).is_err());
        assert!(
            serde_json::from_str::<RpcRequest>(r#"{"type":"snapshot","root_admin":true}"#).is_err()
        );
        assert!(serde_json::from_str::<RpcRequest>(r#"{"type":"independent_admin"}"#).is_err());
    }
}
