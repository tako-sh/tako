//! Single-use enrollment capability. It is never a device's private key.
use crate::api::{RuntimeError, RuntimeResult};
use crate::{DeviceId, EnrollmentRequest, NetworkDescriptor, Permissions, RequestId, ServerInfo};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InvitationToken(Zeroizing<String>);

impl std::fmt::Debug for InvitationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted invitation token]")
    }
}
impl InvitationToken {
    pub fn random() -> RuntimeResult<Self> {
        let mut bytes = Zeroizing::new([0; 32]);
        openssl::rand::rand_bytes(bytes.as_mut()).map_err(crate::Error::from)?;
        Ok(Self(Zeroizing::new(hex::encode(bytes.as_slice()))))
    }
    pub fn hash(&self) -> RuntimeResult<String> {
        if self.0.len() != 64
            || !self
                .0
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(RuntimeError::Authentication);
        }
        Ok(hex::encode(openssl::sha::sha256(self.0.as_bytes())))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvitation {
    pub id: RequestId,
    pub token_hash: String,
    pub permissions: Permissions,
    pub expires_at: u64,
    pub member_expires_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimInvitation {
    pub id: RequestId,
    pub token: InvitationToken,
    pub request: EnrollmentRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    pub version: u8,
    pub network: NetworkDescriptor,
    pub issuer: DeviceId,
    pub server: ServerInfo,
    pub id: RequestId,
    pub token: InvitationToken,
    pub expires_at: u64,
}
impl Invitation {
    pub fn validate(&self) -> RuntimeResult<()> {
        if self.version != 1 || self.server.https_certificate_pem.is_none() {
            return Err(RuntimeError::Invalid("invitation version or server pin"));
        }
        self.server.validate()?;
        self.token.hash()?;
        Ok(())
    }
    pub fn encode(&self) -> RuntimeResult<Zeroizing<String>> {
        self.validate()?;
        let bytes = Zeroizing::new(serde_json::to_vec(self)?);
        Ok(Zeroizing::new(format!(
            "tako-invite-{}",
            URL_SAFE_NO_PAD.encode(bytes.as_slice())
        )))
    }
    pub fn decode(value: &str) -> RuntimeResult<Self> {
        if value.len() > 64 * 1024 {
            return Err(RuntimeError::Capacity("invitation"));
        }
        let value = value
            .trim()
            .strip_prefix("tako-invite-")
            .ok_or(RuntimeError::Invalid("invitation"))?;
        let bytes = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| RuntimeError::Invalid("invitation encoding"))?,
        );
        let invitation: Self = serde_json::from_slice(&bytes)?;
        invitation.validate()?;
        Ok(invitation)
    }
}
