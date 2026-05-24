use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::sign::Signer;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use reqwest::Url;
use sha2::{Digest, Sha256};

const S3_SERVICE: &str = "s3";
const SIGNING_ALGORITHM: &str = "AWS4-HMAC-SHA256";
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
const MAX_EXPIRES_SECONDS: u32 = 7 * 24 * 60 * 60;
const ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

#[derive(Debug, Clone, Copy)]
pub(crate) enum S3Method {
    Get,
    Put,
    Delete,
}

impl S3Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct S3PresignOptions<'a> {
    pub(crate) query: &'a [(&'a str, &'a str)],
    pub(crate) headers: &'a [(&'a str, &'a str)],
    pub(crate) now: time::OffsetDateTime,
}

impl Default for S3PresignOptions<'_> {
    fn default() -> Self {
        Self {
            query: &[],
            headers: &[],
            now: time::OffsetDateTime::now_utc(),
        }
    }
}

struct S3Binding<'a> {
    bucket: &'a str,
    endpoint: &'a str,
    region: &'a str,
    access_key_id: &'a str,
    secret_access_key: &'a str,
    force_path_style: bool,
}

pub(crate) fn presign_s3_url(
    storage: &tako_core::StorageBinding,
    key: &str,
    method: S3Method,
    expires_seconds: u32,
) -> Result<String, String> {
    presign_s3_url_with_options(
        storage,
        key,
        method,
        expires_seconds,
        S3PresignOptions::default(),
    )
}

pub(crate) fn presign_s3_url_with_options(
    storage: &tako_core::StorageBinding,
    key: &str,
    method: S3Method,
    expires_seconds: u32,
    options: S3PresignOptions<'_>,
) -> Result<String, String> {
    let binding = s3_binding(storage)?;
    validate_expires(expires_seconds)?;

    let mut url = object_url(&binding, key)?;
    for (key, value) in options.query {
        url.query_pairs_mut().append_pair(key, value);
    }

    let amz_date = format_amz_date(options.now);
    let date_stamp = &amz_date[..8];
    let scope = format!("{date_stamp}/{}/s3/aws4_request", binding.region);
    let headers = normalize_headers(&url, options.headers)?;
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    {
        let mut query = url.query_pairs_mut();
        query.append_pair("X-Amz-Algorithm", SIGNING_ALGORITHM);
        query.append_pair(
            "X-Amz-Credential",
            &format!("{}/{scope}", binding.access_key_id),
        );
        query.append_pair("X-Amz-Date", &amz_date);
        query.append_pair("X-Amz-Expires", &expires_seconds.to_string());
        query.append_pair("X-Amz-SignedHeaders", &signed_headers);
    }

    let canonical_request = [
        method.as_str().to_string(),
        canonical_uri(&url),
        canonical_query(&url),
        canonical_headers(&headers),
        signed_headers,
        UNSIGNED_PAYLOAD.to_string(),
    ]
    .join("\n");
    let hashed_request = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign = [SIGNING_ALGORITHM, &amz_date, &scope, &hashed_request].join("\n");
    let signing_key = derive_signing_key(binding.secret_access_key, date_stamp, binding.region)?;
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes())?);
    url.query_pairs_mut()
        .append_pair("X-Amz-Signature", &signature);

    Ok(url.to_string())
}

fn s3_binding(storage: &tako_core::StorageBinding) -> Result<S3Binding<'_>, String> {
    if storage.provider != tako_core::StorageProvider::S3 {
        return Err("storage binding is not configured for s3".to_string());
    }
    Ok(S3Binding {
        bucket: require_field(&storage.bucket, "bucket")?,
        endpoint: require_field(&storage.endpoint, "endpoint")?,
        region: require_field(&storage.region, "region")?,
        access_key_id: require_field(&storage.access_key_id, "access_key_id")?,
        secret_access_key: require_field(&storage.secret_access_key, "secret_access_key")?,
        force_path_style: storage.force_path_style,
    })
}

fn require_field<'a>(value: &'a Option<String>, field: &str) -> Result<&'a str, String> {
    let value = value
        .as_deref()
        .ok_or_else(|| format!("storage is missing {field}"))?;
    if value.trim().is_empty() {
        return Err(format!("storage is missing {field}"));
    }
    Ok(value)
}

fn validate_expires(expires_seconds: u32) -> Result<(), String> {
    if expires_seconds == 0 || expires_seconds > MAX_EXPIRES_SECONDS {
        return Err("storage URL expiration must be from 1 to 604800 seconds".to_string());
    }
    Ok(())
}

fn object_url(binding: &S3Binding<'_>, key: &str) -> Result<Url, String> {
    let mut endpoint =
        Url::parse(binding.endpoint).map_err(|e| format!("invalid storage endpoint: {e}"))?;
    if endpoint.scheme() != "https" {
        return Err("storage endpoint must use https".to_string());
    }
    endpoint.set_query(None);
    endpoint.set_fragment(None);

    let encoded_key = encode_object_key(key)?;
    if binding.force_path_style {
        endpoint.set_path(&join_url_path(&[
            endpoint.path(),
            binding.bucket,
            &encoded_key,
        ]));
        return Ok(endpoint);
    }

    let host = endpoint
        .host_str()
        .ok_or_else(|| "storage endpoint must include a host".to_string())?;
    endpoint
        .set_host(Some(&format!("{}.{host}", binding.bucket)))
        .map_err(|_| "invalid bucket host".to_string())?;
    endpoint.set_path(&join_url_path(&[endpoint.path(), &encoded_key]));
    Ok(endpoint)
}

fn encode_object_key(key: &str) -> Result<String, String> {
    if key.trim().is_empty() || key.starts_with('/') {
        return Err("storage key must be a non-empty relative object key".to_string());
    }
    Ok(key
        .split('/')
        .map(|segment| utf8_percent_encode(segment, ENCODE_SET).to_string())
        .collect::<Vec<_>>()
        .join("/"))
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

fn normalize_headers(url: &Url, headers: &[(&str, &str)]) -> Result<Vec<(String, String)>, String> {
    let mut normalized = vec![("host".to_string(), canonical_host(url)?)];
    for (name, value) in headers {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err("storage signature header name cannot be empty".to_string());
        }
        if name == "host" {
            return Err("storage signature host header is derived from endpoint".to_string());
        }
        normalized.push((name, normalize_header_value(value)));
    }
    normalized.sort_by(|a, b| a.0.cmp(&b.0));
    for pair in normalized.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(format!(
                "storage signature header '{}' is duplicated",
                pair[0].0
            ));
        }
    }
    Ok(normalized)
}

fn normalize_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn canonical_headers(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>()
}

fn canonical_uri(url: &Url) -> String {
    url.path()
        .split('/')
        .map(|segment| {
            let decoded = percent_decode_str(segment).decode_utf8_lossy();
            utf8_percent_encode(&decoded, ENCODE_SET).to_string()
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_query(url: &Url) -> String {
    let mut pairs = url.query_pairs().into_owned().collect::<Vec<_>>();
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    pairs
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                utf8_percent_encode(&key, ENCODE_SET),
                utf8_percent_encode(&value, ENCODE_SET)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn canonical_host(url: &Url) -> Result<String, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "storage endpoint must include a host".to_string())?;
    match url.port() {
        Some(port) => Ok(format!("{host}:{port}")),
        None => Ok(host.to_string()),
    }
}

fn format_amz_date(value: time::OffsetDateTime) -> String {
    let date = value.date();
    let time = value.time();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

fn derive_signing_key(secret: &str, date_stamp: &str, region: &str) -> Result<Vec<u8>, String> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes())?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, S3_SERVICE.as_bytes())?;
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> Result<Vec<u8>, String> {
    let key = PKey::hmac(key).map_err(|e| format!("create hmac key: {e}"))?;
    let mut signer = Signer::new(MessageDigest::sha256(), &key)
        .map_err(|e| format!("create hmac signer: {e}"))?;
    signer
        .update(value)
        .map_err(|e| format!("update hmac signer: {e}"))?;
    signer
        .sign_to_vec()
        .map_err(|e| format!("finalize hmac signer: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_storage() -> tako_core::StorageBinding {
        tako_core::StorageBinding {
            provider: tako_core::StorageProvider::S3,
            bucket: Some("demo-backups".to_string()),
            endpoint: Some("https://s3.example.com".to_string()),
            region: Some("us-east-1".to_string()),
            access_key_id: Some("key-id".to_string()),
            secret_access_key: Some("secret".to_string()),
            force_path_style: false,
            public_base_url: Some("https://cdn.example.com/backups".to_string()),
            path: None,
            signing_key: None,
        }
    }

    fn fixed_now() -> time::OffsetDateTime {
        time::Date::from_calendar_date(2026, time::Month::May, 13)
            .unwrap()
            .with_hms(12, 34, 56)
            .unwrap()
            .assume_utc()
    }

    #[test]
    fn presign_s3_url_uses_virtual_hosted_object_url() {
        let url = presign_s3_url_with_options(
            &test_storage(),
            "_tako/backups/demo/production/la/b1.tar.zst",
            S3Method::Put,
            900,
            S3PresignOptions {
                now: fixed_now(),
                ..Default::default()
            },
        )
        .unwrap();

        let parsed = Url::parse(&url).unwrap();
        assert_eq!(
            format!(
                "{}://{}{}",
                parsed.scheme(),
                parsed.host_str().unwrap(),
                parsed.path()
            ),
            "https://demo-backups.s3.example.com/_tako/backups/demo/production/la/b1.tar.zst"
        );
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "X-Amz-Algorithm"),
            Some(("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()))
        );
        assert_eq!(
            parsed.query_pairs().find(|(key, _)| key == "X-Amz-Date"),
            Some(("X-Amz-Date".into(), "20260513T123456Z".into()))
        );
    }

    #[test]
    fn presign_s3_url_signs_extra_headers_and_query_params() {
        let headers = [("content-type", "application/json")];
        let query = [("response-content-type", "application/json")];
        let url = presign_s3_url_with_options(
            &test_storage(),
            "manifest.json",
            S3Method::Get,
            900,
            S3PresignOptions {
                query: &query,
                headers: &headers,
                now: fixed_now(),
            },
        )
        .unwrap();

        let parsed = Url::parse(&url).unwrap();
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "X-Amz-SignedHeaders"),
            Some(("X-Amz-SignedHeaders".into(), "content-type;host".into()))
        );
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "response-content-type"),
            Some(("response-content-type".into(), "application/json".into()))
        );
    }

    #[test]
    fn presign_s3_url_uses_path_style_when_requested() {
        let storage = tako_core::StorageBinding {
            force_path_style: true,
            endpoint: Some("https://minio.example.com/root".to_string()),
            ..test_storage()
        };

        let url = presign_s3_url_with_options(
            &storage,
            "data/a b.tar.zst",
            S3Method::Get,
            900,
            S3PresignOptions {
                now: fixed_now(),
                ..Default::default()
            },
        )
        .unwrap();

        let parsed = Url::parse(&url).unwrap();
        assert_eq!(
            format!(
                "{}://{}{}",
                parsed.scheme(),
                parsed.host_str().unwrap(),
                parsed.path()
            ),
            "https://minio.example.com/root/demo-backups/data/a%20b.tar.zst"
        );
    }

    #[test]
    fn presign_s3_url_rejects_absolute_object_keys() {
        let err = presign_s3_url(&test_storage(), "/data.tar.zst", S3Method::Get, 900).unwrap_err();

        assert_eq!(
            err,
            "storage key must be a non-empty relative object key".to_string()
        );
    }
}
