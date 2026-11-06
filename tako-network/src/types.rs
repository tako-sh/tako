use std::{fmt, net::Ipv6Addr, str::FromStr};

use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::{
    derive::Deriver,
    pkey::{Id, PKey},
    sha::sha256,
};
use serde::{Deserialize, Serialize};
use ssh_key::rand_core::{OsRng, RngCore};

use crate::{Result, error::invalid};

macro_rules! hex_id {
    ($name:ident, $size:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name([u8; $size]);

        impl $name {
            pub fn as_bytes(&self) -> &[u8; $size] {
                &self.0
            }
        }
        impl FromStr for $name {
            type Err = crate::Error;
            fn from_str(value: &str) -> Result<Self> {
                if value.len() != $size * 2
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(invalid(
                        stringify!($name),
                        "expected fixed-length lowercase hex",
                    ));
                }
                let mut bytes = [0; $size];
                hex::decode_to_slice(value, &mut bytes)
                    .map_err(|_| invalid(stringify!($name), "invalid hex"))?;
                Ok(Self(bytes))
            }
        }
        impl TryFrom<String> for $name {
            type Error = crate::Error;
            fn try_from(value: String) -> Result<Self> {
                value.parse()
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.to_string()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }
    };
}

hex_id!(NetworkId, 16);
hex_id!(RequestId, 16);
hex_id!(RevocationId, 16);
hex_id!(BootId, 16);
hex_id!(DeviceId, 32);
hex_id!(KeyFingerprint, 32);

macro_rules! random_id {
    ($($name:ident),*) => {$ (
        impl $name {
            pub fn random() -> Result<Self> {
                let mut bytes = [0; 16];
                OsRng.try_fill_bytes(&mut bytes).map_err(|_| invalid("randomness", "OS entropy unavailable"))?;
                Ok(Self(bytes))
            }
        }
    )*};
}
random_id!(NetworkId, RequestId, RevocationId, BootId);

impl NetworkId {
    pub fn address(&self, key: &WgPublicKey) -> Ipv6Addr {
        let mut network_input = b"tako-network-prefix-v1\0".to_vec();
        network_input.extend_from_slice(self.as_bytes());
        let network_hash = sha256(&network_input);
        let mut host_input = b"tako-network-host-v1\0".to_vec();
        host_input.extend_from_slice(self.as_bytes());
        host_input.extend_from_slice(key.as_bytes());
        let host_hash = sha256(&host_input);
        let mut bytes = [0; 16];
        bytes[0] = 0xfd;
        bytes[1..6].copy_from_slice(&network_hash[..5]);
        bytes[6..].copy_from_slice(&host_hash[..10]);
        Ipv6Addr::from(bytes)
    }
}

impl DeviceId {
    pub fn from_keys(
        network: &NetworkId,
        kind: DeviceKind,
        ssh: &SshPublicKey,
        wg: &WgPublicKey,
    ) -> Result<Self> {
        let mut bytes = b"tako-device-v1\0".to_vec();
        bytes.extend_from_slice(network.as_bytes());
        bytes.push(kind.tag());
        bytes.extend_from_slice(&ssh.parsed()?.to_bytes()?);
        bytes.extend_from_slice(wg.as_bytes());
        Ok(Self(sha256(&bytes)))
    }
}

impl KeyFingerprint {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(sha256(bytes))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WgPublicKey([u8; 32]);

impl WgPublicKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn fingerprint(&self) -> KeyFingerprint {
        KeyFingerprint::of_bytes(&self.0)
    }
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self> {
        let mut prime = [0xff; 32];
        prime[0] = 0xed;
        prime[31] = 0x7f;
        if bytes.iter().rev().cmp(prime.iter().rev()) != std::cmp::Ordering::Less {
            return Err(invalid("WG public key", "noncanonical X25519 coordinate"));
        }
        let public = PKey::public_key_from_raw_bytes(&bytes, Id::X25519)?;
        // A public, fixed scalar tests low-order points using OpenSSL's X25519 implementation.
        let probe = PKey::private_key_from_raw_bytes(&[0x42; 32], Id::X25519)?;
        let mut derive = Deriver::new(&probe)?;
        derive
            .set_peer(&public)
            .map_err(|_| invalid("WG public key", "invalid X25519 point"))?;
        let shared = derive
            .derive_to_vec()
            .map_err(|_| invalid("WG public key", "low-order X25519 point"))?;
        if shared.iter().all(|b| *b == 0) {
            return Err(invalid("WG public key", "low-order X25519 point"));
        }
        Ok(Self(bytes))
    }
}
impl FromStr for WgPublicKey {
    type Err = crate::Error;
    fn from_str(value: &str) -> Result<Self> {
        if value.len() != 44 {
            return Err(invalid("WG public key", "expected 32-byte padded base64"));
        }
        let bytes = STANDARD
            .decode(value)
            .map_err(|_| invalid("WG public key", "invalid base64"))?;
        if STANDARD.encode(&bytes) != value {
            return Err(invalid("WG public key", "noncanonical base64"));
        }
        Self::from_bytes(
            bytes
                .try_into()
                .map_err(|_| invalid("WG public key", "expected 32 bytes"))?,
        )
    }
}
impl TryFrom<String> for WgPublicKey {
    type Error = crate::Error;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}
impl From<WgPublicKey> for String {
    fn from(value: WgPublicKey) -> Self {
        value.to_string()
    }
}
impl fmt::Display for WgPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&STANDARD.encode(self.0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SshPublicKey(String);

impl SshPublicKey {
    pub(crate) fn parsed(&self) -> Result<ssh_key::PublicKey> {
        Ok(ssh_key::PublicKey::from_openssh(&self.0)?)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn fingerprint(&self) -> Result<KeyFingerprint> {
        Ok(KeyFingerprint::of_bytes(&self.parsed()?.to_bytes()?))
    }
}
impl FromStr for SshPublicKey {
    type Err = crate::Error;
    fn from_str(value: &str) -> Result<Self> {
        if value.len() > 128 {
            return Err(invalid("SSH public key", "key too long"));
        }
        let key = ssh_key::PublicKey::from_openssh(value)?;
        if key.algorithm() != ssh_key::Algorithm::Ed25519
            || !key.comment().is_empty()
            || key.to_openssh()? != value
        {
            return Err(invalid(
                "SSH public key",
                "expected canonical comment-free Ed25519 key",
            ));
        }
        let ed25519 = key
            .key_data()
            .ed25519()
            .ok_or_else(|| invalid("SSH public key", "expected Ed25519 key"))?;
        let verifier = ed25519_dalek::VerifyingKey::from_bytes(&ed25519.0)
            .map_err(|_| invalid("SSH public key", "invalid Ed25519 point"))?;
        if verifier.is_weak() {
            return Err(invalid("SSH public key", "weak Ed25519 point"));
        }
        Ok(Self(value.into()))
    }
}
impl TryFrom<String> for SshPublicKey {
    type Error = crate::Error;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}
impl From<SshPublicKey> for String {
    fn from(value: SshPublicKey) -> Self {
        value.0
    }
}
impl fmt::Display for SshPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! text_type {
    ($name:ident, $validate:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl FromStr for $name {
            type Err = crate::Error;
            fn from_str(value: &str) -> Result<Self> {
                if !($validate)(value) {
                    return Err(invalid(stringify!($name), "invalid canonical value"));
                }
                Ok(Self(value.into()))
            }
        }
        impl TryFrom<String> for $name {
            type Error = crate::Error;
            fn try_from(value: String) -> Result<Self> {
                value.parse()
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
fn segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}
text_type!(DeviceName, |s: &str| !s.is_empty()
    && s.len() <= 63
    && s.as_bytes()[0].is_ascii_alphanumeric()
    && s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)));
text_type!(SshUser, |s: &str| segment(s) && s.len() <= 32);
text_type!(AppEnv, |s: &str| s
    .split_once('/')
    .is_some_and(|(app, env)| segment(app) && segment(env)));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeviceKind {
    Laptop,
    Server,
}
impl DeviceKind {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Self::Laptop => 0,
            Self::Server => 1,
        }
    }
}
