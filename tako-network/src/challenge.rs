use crate::{
    DeviceId, EnrollmentRequest, KeyFingerprint, LocalDeviceKeys, NetworkId, RequestId, Result,
    WgPublicKey, error::invalid,
};
use openssl::{
    derive::Deriver,
    hash::MessageDigest,
    memcmp,
    pkey::{Id, PKey, Private},
    sign::Signer,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WgChallenge {
    pub network_id: NetworkId,
    pub device_id: DeviceId,
    pub request_digest: KeyFingerprint,
    pub nonce: RequestId,
    pub ephemeral_public_key: WgPublicKey,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WgProof {
    pub response: KeyFingerprint,
}

pub struct PendingWgChallenge {
    challenge: WgChallenge,
    private: PKey<Private>,
    request: EnrollmentRequest,
}

/// Created only after the server verifies possession of the requested WG private key.
/// It is deliberately neither deserializable nor publicly constructible.
#[derive(Clone, Debug)]
pub struct VerifiedEnrollment {
    pub(crate) request: EnrollmentRequest,
    pub(crate) network_id: NetworkId,
    pub(crate) device_id: DeviceId,
}

impl VerifiedEnrollment {
    pub fn request(&self) -> &EnrollmentRequest {
        &self.request
    }

    pub fn network_id(&self) -> &NetworkId {
        &self.network_id
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

impl WgChallenge {
    pub fn begin(
        request: &EnrollmentRequest,
        network_id: NetworkId,
        now: u64,
        expires_at: u64,
    ) -> Result<(Self, PendingWgChallenge)> {
        request.verify()?;
        if expires_at <= now
            || expires_at.saturating_sub(now) > 300
            || request.expires_at.is_some_and(|expiry| expiry <= now)
        {
            return Err(invalid(
                "challenge expiry",
                "must be live and at most 300 seconds",
            ));
        }
        let private = PKey::generate_x25519()?;
        let challenge = Self {
            device_id: request.device_id_for(&network_id)?,
            network_id,
            request_digest: KeyFingerprint::of_bytes(&request.canonical_bytes()?),
            nonce: RequestId::random()?,
            ephemeral_public_key: WgPublicKey::from_bytes(
                private
                    .raw_public_key()?
                    .try_into()
                    .map_err(|_| invalid("challenge", "invalid public key size"))?,
            )?,
            expires_at,
        };
        Ok((
            challenge.clone(),
            PendingWgChallenge {
                challenge,
                private,
                request: request.clone(),
            },
        ))
    }

    fn transcript(&self) -> Vec<u8> {
        let mut bytes = b"tako-wg-possession-v1\0".to_vec();
        bytes.extend_from_slice(self.network_id.as_bytes());
        bytes.extend_from_slice(self.device_id.as_bytes());
        bytes.extend_from_slice(self.request_digest.as_bytes());
        bytes.extend_from_slice(self.nonce.as_bytes());
        bytes.extend_from_slice(self.ephemeral_public_key.as_bytes());
        bytes.extend_from_slice(&self.expires_at.to_be_bytes());
        bytes
    }
}

impl LocalDeviceKeys {
    /// The caller authenticates the server channel before answering a challenge.
    pub fn answer_challenge(
        &self,
        challenge: &WgChallenge,
        request: &EnrollmentRequest,
        now: u64,
    ) -> Result<WgProof> {
        request.verify()?;
        if challenge.expires_at <= now
            || request.expires_at.is_some_and(|expiry| expiry <= now)
            || request.wg_public_key != self.wg_public_key()?
            || request.ssh_public_key != self.ssh_public_key()?
            || challenge.device_id != request.device_id_for(&challenge.network_id)?
            || challenge.request_digest != KeyFingerprint::of_bytes(&request.canonical_bytes()?)
        {
            return Err(invalid(
                "WG challenge",
                "expired or does not bind this request and device",
            ));
        }
        Ok(WgProof {
            response: response(
                &self.wg,
                &challenge.ephemeral_public_key,
                &challenge.transcript(),
            )?,
        })
    }
}

impl PendingWgChallenge {
    pub fn verify(self, proof: &WgProof, now: u64) -> Result<VerifiedEnrollment> {
        if self.challenge.expires_at <= now
            || self.request.expires_at.is_some_and(|expiry| expiry <= now)
        {
            return Err(invalid("WG challenge", "expired"));
        }
        let expected = response(
            &self.private,
            &self.request.wg_public_key,
            &self.challenge.transcript(),
        )?;
        if !memcmp::eq(expected.as_bytes(), proof.response.as_bytes()) {
            return Err(invalid(
                "WG proof",
                "private-key possession not demonstrated",
            ));
        }
        Ok(VerifiedEnrollment {
            request: self.request,
            network_id: self.challenge.network_id,
            device_id: self.challenge.device_id,
        })
    }
}

fn response(
    private: &PKey<Private>,
    public: &WgPublicKey,
    transcript: &[u8],
) -> Result<KeyFingerprint> {
    let peer = PKey::public_key_from_raw_bytes(public.as_bytes(), Id::X25519)?;
    let mut derive = Deriver::new(private)?;
    derive.set_peer(&peer)?;
    let shared = Zeroizing::new(derive.derive_to_vec()?);
    let hmac_key = PKey::hmac(&shared)?;
    let mut signer = Signer::new(MessageDigest::sha256(), &hmac_key)?;
    signer.update(transcript)?;
    let mac = Zeroizing::new(signer.sign_to_vec()?);
    hex::encode(mac.as_slice()).parse()
}
