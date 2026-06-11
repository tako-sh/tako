use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};
use url::Url;

pub const DEFAULT_STORAGE_URL_EXPIRES_SECONDS: u64 = 3600;
pub const MAX_STORAGE_URL_EXPIRES_SECONDS: u64 = 7 * 24 * 60 * 60;
const S3_SERVICE: &str = "s3";
const S3_SIGNING_ALGORITHM: &str = "AWS4-HMAC-SHA256";
const S3_UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

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
        self.create_download_url_at(key, options, SystemTime::now())
    }

    fn create_download_url_at(
        &self,
        key: &str,
        options: UrlOptions,
        now: SystemTime,
    ) -> Result<String, Error> {
        match &self.binding {
            StorageBinding::Local { signing_key, .. } => {
                self.local_url_at("GET", key, signing_key, options.expires_in_seconds, now)
            }
            StorageBinding::S3 {
                public_base_url: Some(base),
                ..
            } if options.public => join_public_url(base, key),
            StorageBinding::S3 { .. } => self.s3_url("GET", key, options, now),
        }
    }

    pub fn create_upload_url(&self, key: &str, options: UrlOptions) -> Result<String, Error> {
        self.create_upload_url_at(key, options, SystemTime::now())
    }

    fn create_upload_url_at(
        &self,
        key: &str,
        options: UrlOptions,
        now: SystemTime,
    ) -> Result<String, Error> {
        match &self.binding {
            StorageBinding::Local { signing_key, .. } => {
                self.local_url_at("PUT", key, signing_key, options.expires_in_seconds, now)
            }
            StorageBinding::S3 { .. } => self.s3_url("PUT", key, options, now),
        }
    }

    pub fn binding(&self) -> &StorageBinding {
        &self.binding
    }

    fn local_url_at(
        &self,
        method: &str,
        key: &str,
        signing_key: &str,
        expires_in_seconds: Option<u64>,
        now: SystemTime,
    ) -> Result<String, Error> {
        let expires_in = validate_expires(expires_in_seconds)?;
        let expires = unix_secs(now) + expires_in;
        let encoded_key = encode_object_key(key)?;
        let payload = format!("{method}\n{}\n{encoded_key}\n{expires}", self.name);
        let token = hmac_hex(signing_key.as_bytes(), payload.as_bytes());
        Ok(format!(
            "/_tako/storages/{}/{encoded_key}?expires={expires}&token={token}",
            encode_component(&self.name)
        ))
    }

    fn s3_url(
        &self,
        method: &str,
        key: &str,
        options: UrlOptions,
        now: SystemTime,
    ) -> Result<String, Error> {
        let StorageBinding::S3 {
            bucket,
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            force_path_style,
            ..
        } = &self.binding
        else {
            unreachable!("s3_url called only for s3 bindings");
        };
        let expires = validate_expires(options.expires_in_seconds)?;
        let mut url = s3_object_url(endpoint, bucket, key, *force_path_style)?;

        if let Some(content_type) = &options.response_content_type {
            url.query_pairs_mut()
                .append_pair("response-content-type", content_type);
        }
        if let Some(disposition) = &options.response_content_disposition {
            url.query_pairs_mut()
                .append_pair("response-content-disposition", disposition);
        }

        let amz_date = format_amz_date(now);
        let date_stamp = amz_date[..8].to_string();
        let scope = format!("{date_stamp}/{region}/{S3_SERVICE}/aws4_request");
        let mut headers = HashMap::from([(
            "host".to_string(),
            normalize_header_value(&host_header(&url)),
        )]);
        if method == "PUT"
            && let Some(content_type) = &options.content_type
        {
            headers.insert(
                "content-type".to_string(),
                normalize_header_value(content_type),
            );
        }
        let signed_headers = signed_headers(&headers);

        {
            let mut query = url.query_pairs_mut();
            query.append_pair("X-Amz-Algorithm", S3_SIGNING_ALGORITHM);
            query.append_pair("X-Amz-Credential", &format!("{access_key_id}/{scope}"));
            query.append_pair("X-Amz-Date", &amz_date);
            query.append_pair("X-Amz-Expires", &expires.to_string());
            query.append_pair("X-Amz-SignedHeaders", &signed_headers);
        }

        let canonical_request = [
            method.to_string(),
            canonical_uri(&url),
            canonical_query(&url),
            canonical_headers(&headers),
            signed_headers,
            S3_UNSIGNED_PAYLOAD.to_string(),
        ]
        .join("\n");
        let string_to_sign = [
            S3_SIGNING_ALGORITHM.to_string(),
            amz_date,
            scope,
            sha256_hex(canonical_request.as_bytes()),
        ]
        .join("\n");
        let signing_key = derive_s3_signing_key(secret_access_key, &date_stamp, region);
        let signature = hmac_hex(&signing_key, string_to_sign.as_bytes());
        url.query_pairs_mut()
            .append_pair("X-Amz-Signature", &signature);
        Ok(url.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct UrlOptions {
    pub expires_in_seconds: Option<u64>,
    pub public: bool,
    pub content_type: Option<String>,
    pub response_content_type: Option<String>,
    pub response_content_disposition: Option<String>,
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
    hex::encode(hmac_bytes(key, payload))
}

fn hmac_bytes(key: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(payload: &[u8]) -> String {
    hex::encode(Sha256::digest(payload))
}

fn join_public_url(base: &str, key: &str) -> Result<String, Error> {
    Ok(format!(
        "{}/{}",
        base.trim_end_matches('/'),
        encode_object_key(key)?.trim_start_matches('/')
    ))
}

fn encode_object_key(key: &str) -> Result<String, Error> {
    if key.trim().is_empty() || key.starts_with('/') {
        return Err(Error::InvalidKey);
    }
    Ok(encode_relative_path(key))
}

fn encode_relative_path(key: &str) -> String {
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

fn validate_expires(expires_in_seconds: Option<u64>) -> Result<u64, Error> {
    let expires = expires_in_seconds.unwrap_or(DEFAULT_STORAGE_URL_EXPIRES_SECONDS);
    if expires == 0 || expires > MAX_STORAGE_URL_EXPIRES_SECONDS {
        return Err(Error::InvalidExpiry);
    }
    Ok(expires)
}

fn unix_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn s3_object_url(
    endpoint: &str,
    bucket: &str,
    key: &str,
    force_path_style: bool,
) -> Result<Url, Error> {
    let encoded_key = encode_object_key(key)?;
    let mut url = Url::parse(endpoint).map_err(|_| Error::InvalidEndpoint(endpoint.to_string()))?;
    if url.scheme() != "https" {
        return Err(Error::InvalidEndpoint(endpoint.to_string()));
    }
    if force_path_style {
        let path = join_url_path(&[url.path(), bucket, &encoded_key]);
        url.set_path(&path);
        return Ok(url);
    }
    let host = url
        .host_str()
        .ok_or_else(|| Error::InvalidEndpoint(endpoint.to_string()))?;
    url.set_host(Some(&format!("{bucket}.{host}")))
        .map_err(|_| Error::InvalidEndpoint(endpoint.to_string()))?;
    let path = join_url_path(&[url.path(), &encoded_key]);
    url.set_path(&path);
    Ok(url)
}

fn join_url_path(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .map(|part| part.trim_matches('/'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    format!("/{joined}")
}

fn normalize_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn signed_headers(headers: &HashMap<String, String>) -> String {
    let mut keys = headers.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys.join(";")
}

fn canonical_headers(headers: &HashMap<String, String>) -> String {
    let mut entries = headers.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(key, _)| *key);
    entries
        .into_iter()
        .map(|(key, value)| format!("{key}:{value}\n"))
        .collect()
}

fn canonical_uri(url: &Url) -> String {
    url.path()
        .split('/')
        .map(|segment| encode_component(&percent_decode(segment)))
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_query(url: &Url) -> String {
    let mut entries = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    entries.sort_by(|(a_key, a_value), (b_key, b_value)| {
        a_key.cmp(b_key).then_with(|| a_value.cmp(b_value))
    });
    entries
        .into_iter()
        .map(|(key, value)| format!("{}={}", encode_component(&key), encode_component(&value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn host_header(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => String::new(),
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(value) = u8::from_str_radix(&input[i + 1..i + 3], 16)
        {
            out.push(value);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn derive_s3_signing_key(secret: &str, date_stamp: &str, region: &str) -> Vec<u8> {
    let date_key = hmac_bytes(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let region_key = hmac_bytes(&date_key, region.as_bytes());
    let service_key = hmac_bytes(&region_key, S3_SERVICE.as_bytes());
    hmac_bytes(&service_key, b"aws4_request")
}

fn format_amz_date(now: SystemTime) -> String {
    let seconds = unix_secs(now) as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage key must be a non-empty relative object key")]
    InvalidKey,
    #[error("storage URL expiration must be an integer from 1 to 604800 seconds")]
    InvalidExpiry,
    #[error("storage endpoint must be a valid https URL: {0}")]
    InvalidEndpoint(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_clock() -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(1_778_675_696)
    }

    fn s3_storage(force_path_style: bool) -> Storage {
        Storage::new(
            "uploads",
            StorageBinding::S3 {
                bucket: "app-uploads".to_string(),
                endpoint: "https://abc.r2.cloudflarestorage.com".to_string(),
                region: "auto".to_string(),
                access_key_id: "test-key".to_string(),
                secret_access_key: "test-secret".to_string(),
                force_path_style,
                public_base_url: Some("https://cdn.example.com/uploads".to_string()),
            },
        )
    }

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
                    ..UrlOptions::default()
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
                    ..UrlOptions::default()
                },
            )
            .unwrap();

        assert_eq!(url, "https://cdn.example.com/base/nested/file%20name.txt");
    }

    #[test]
    fn s3_download_url_uses_sigv4_query_signing() {
        let storage = s3_storage(false);

        let url = storage
            .create_download_url_at(
                "receipts/r 123.png",
                UrlOptions {
                    expires_in_seconds: Some(3600),
                    response_content_type: Some("image/png".to_string()),
                    ..UrlOptions::default()
                },
                fixed_clock(),
            )
            .unwrap();

        assert!(
            url.starts_with(
                "https://app-uploads.abc.r2.cloudflarestorage.com/receipts/r%20123.png?"
            )
        );
        let parsed = Url::parse(&url).unwrap();
        let query = parsed.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            query.get("X-Amz-Algorithm").map(|v| v.as_ref()),
            Some("AWS4-HMAC-SHA256")
        );
        assert_eq!(
            query.get("X-Amz-Credential").map(|v| v.as_ref()),
            Some("test-key/20260513/auto/s3/aws4_request")
        );
        assert_eq!(
            query.get("X-Amz-Date").map(|v| v.as_ref()),
            Some("20260513T123456Z")
        );
        assert_eq!(query.get("X-Amz-Expires").map(|v| v.as_ref()), Some("3600"));
        assert_eq!(
            query.get("X-Amz-SignedHeaders").map(|v| v.as_ref()),
            Some("host")
        );
        assert_eq!(
            query.get("response-content-type").map(|v| v.as_ref()),
            Some("image/png")
        );
        assert_eq!(
            query.get("X-Amz-Signature").map(|v| v.as_ref()),
            Some("314b05cbab13c08d06ad922dce295280c00bb0da327735b43e5ba7ae8402210b")
        );
    }

    #[test]
    fn s3_upload_url_signs_content_type_header() {
        let storage = s3_storage(false);

        let url = storage
            .create_upload_url_at(
                "avatars/u_123.png",
                UrlOptions {
                    content_type: Some("image/png".to_string()),
                    ..UrlOptions::default()
                },
                fixed_clock(),
            )
            .unwrap();

        let parsed = Url::parse(&url).unwrap();
        let query = parsed.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            query.get("X-Amz-SignedHeaders").map(|v| v.as_ref()),
            Some("content-type;host")
        );
        assert_eq!(
            query.get("X-Amz-Signature").map(|v| v.as_ref()),
            Some("12ad576d23a91feb868e6e395a92e0cb0f0321262085693838f5dcdd330b8b76")
        );
    }

    #[test]
    fn s3_path_style_url_places_bucket_in_path() {
        let storage = s3_storage(true);

        let url = storage
            .create_download_url_at("nested/file.txt", UrlOptions::default(), fixed_clock())
            .unwrap();

        assert!(
            url.starts_with("https://abc.r2.cloudflarestorage.com/app-uploads/nested/file.txt?")
        );
    }

    #[test]
    fn storage_urls_reject_invalid_keys_and_expiry() {
        let storage = s3_storage(false);

        assert!(matches!(
            storage.create_download_url("", UrlOptions::default()),
            Err(Error::InvalidKey)
        ));
        assert!(matches!(
            storage.create_download_url(
                "/absolute",
                UrlOptions {
                    ..UrlOptions::default()
                }
            ),
            Err(Error::InvalidKey)
        ));
        assert!(matches!(
            storage.create_download_url(
                "ok",
                UrlOptions {
                    expires_in_seconds: Some(MAX_STORAGE_URL_EXPIRES_SECONDS + 1),
                    ..UrlOptions::default()
                }
            ),
            Err(Error::InvalidExpiry)
        ));
    }
}
