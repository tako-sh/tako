use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use std::collections::HashMap;

pub const DEFAULT_STORAGE_URL_EXPIRES_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum StorageBinding {
    Local {
        path: String,
        signing_key: String,
    },
    S3 {
        bucket: String,
        endpoint: String,
        region: String,
        access_key_id: String,
        secret_access_key: String,
        #[serde(default)]
        force_path_style: bool,
        #[serde(default)]
        public_base_url: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Storage {
    name: String,
    binding: StorageBinding,
}

impl Storage {
    pub fn new(name: impl Into<String>, binding: StorageBinding) -> Self {
        Self {
            name: name.into(),
            binding,
        }
    }

    pub fn create_download_url(&self, key: &str, options: UrlOptions) -> Result<String, Error> {
        match &self.binding {
            StorageBinding::Local { signing_key, .. } => {
                self.local_url("GET", key, signing_key, options.expires_in_seconds)
            }
            StorageBinding::S3 {
                public_base_url: Some(base),
                ..
            } if options.public => Ok(join_public_url(base, key)),
            StorageBinding::S3 { .. } => Err(Error::PrivateS3SigningUnsupported),
        }
    }

    pub fn create_upload_url(&self, key: &str, options: UrlOptions) -> Result<String, Error> {
        match &self.binding {
            StorageBinding::Local { signing_key, .. } => {
                self.local_url("PUT", key, signing_key, options.expires_in_seconds)
            }
            StorageBinding::S3 { .. } => Err(Error::PrivateS3SigningUnsupported),
        }
    }

    pub fn binding(&self) -> &StorageBinding {
        &self.binding
    }

    fn local_url(
        &self,
        method: &str,
        key: &str,
        signing_key: &str,
        expires_in_seconds: Option<u64>,
    ) -> Result<String, Error> {
        let expires_in = expires_in_seconds.unwrap_or(DEFAULT_STORAGE_URL_EXPIRES_SECONDS);
        if expires_in == 0 {
            return Err(Error::InvalidExpiry);
        }
        let expires = unix_now_secs() + expires_in;
        let encoded_key = encode_object_key(key);
        let payload = format!("{method}\n{}\n{encoded_key}\n{expires}", self.name);
        let token = hmac_hex(signing_key.as_bytes(), payload.as_bytes());
        Ok(format!(
            "/_tako/storages/{}/{encoded_key}?expires={expires}&token={token}",
            encode_component(&self.name)
        ))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UrlOptions {
    pub expires_in_seconds: Option<u64>,
    pub public: bool,
}

#[derive(Debug, Clone)]
pub struct StorageBag {
    storages: HashMap<String, Storage>,
}

impl StorageBag {
    pub fn from_value(value: &serde_json::Value) -> Result<Self, Error> {
        let bindings: HashMap<String, StorageBinding> = serde_json::from_value(value.clone())?;
        Ok(Self {
            storages: bindings
                .into_iter()
                .map(|(name, binding)| {
                    let storage = Storage::new(name.clone(), binding);
                    (name, storage)
                })
                .collect(),
        })
    }

    pub fn get(&self, name: &str) -> Option<&Storage> {
        self.storages.get(name)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.storages.contains_key(name)
    }
}

fn hmac_hex(key: &[u8], payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

fn join_public_url(base: &str, key: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        encode_object_key(key).trim_start_matches('/')
    )
}

fn encode_object_key(key: &str) -> String {
    key.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_component(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("URL expiry must be greater than zero")]
    InvalidExpiry,
    #[error("private S3 signing is not available in the Rust SDK yet")]
    PrivateS3SigningUnsupported,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_storage_bag_from_bootstrap_value() {
        let bag = StorageBag::from_value(&serde_json::json!({
            "uploads": {
                "provider": "local",
                "path": "/data/uploads",
                "signing_key": "secret"
            }
        }))
        .unwrap();

        assert!(bag.contains_key("uploads"));
    }

    #[test]
    fn local_download_url_matches_tako_storage_route_shape() {
        let storage = Storage::new(
            "user uploads",
            StorageBinding::Local {
                path: "/data/uploads".to_string(),
                signing_key: "secret".to_string(),
            },
        );

        let url = storage
            .create_download_url(
                "avatars/a b.png",
                UrlOptions {
                    expires_in_seconds: Some(60),
                    public: false,
                },
            )
            .unwrap();

        assert!(url.starts_with("/_tako/storages/user%20uploads/avatars/a%20b.png?"));
        assert!(url.contains("expires="));
        assert!(url.contains("token="));
    }

    #[test]
    fn s3_public_url_uses_public_base_url() {
        let storage = Storage::new(
            "assets",
            StorageBinding::S3 {
                bucket: "assets".to_string(),
                endpoint: "https://s3.example.com".to_string(),
                region: "auto".to_string(),
                access_key_id: "key".to_string(),
                secret_access_key: "secret".to_string(),
                force_path_style: true,
                public_base_url: Some("https://cdn.example.com/base/".to_string()),
            },
        );

        let url = storage
            .create_download_url(
                "nested/file name.txt",
                UrlOptions {
                    expires_in_seconds: None,
                    public: true,
                },
            )
            .unwrap();

        assert_eq!(url, "https://cdn.example.com/base/nested/file%20name.txt");
    }
}
