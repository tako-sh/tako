//! Signed local storage URLs served by Tako's Rust proxies, independent of app workers.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::TryStreamExt;
use object_store::{ObjectStoreExt, buffered::BufWriter, path::Path as ObjectPath};
use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};
use percent_encoding::percent_decode_str;
use pingora_core::{Error, ErrorType, Result};
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use tako_core::{StorageBinding, StorageProvider};
use tokio::io::AsyncWriteExt;

/// The SDK creates URLs at this path; only the Rust proxies serve them.
pub const PREFIX: &str = "/_tako/storages/";
/// Match the production proxy's existing upload limit, including chunked bodies.
pub const MAX_BODY_BYTES: u64 = 128 * 1024 * 1024;

/// Local bindings and directories for one application selected by the proxy.
#[derive(Clone, Debug)]
pub struct LocalStorage {
    pub bindings: HashMap<String, StorageBinding>,
    pub root: PathBuf,
    pub legacy_root: PathBuf,
}

impl LocalStorage {
    pub async fn serve(&self, session: &mut Session) -> Result<bool> {
        serve(session, &self.bindings, &self.root, &self.legacy_root).await
    }
}

/// Serve one local-storage request using the matched app's bindings and private data root.
/// Existing SDK files are imported from `legacy_root` before the first operation.
pub async fn serve(
    session: &mut Session,
    bindings: &HashMap<String, StorageBinding>,
    root: &Path,
    legacy_root: &Path,
) -> Result<bool> {
    let uri = session.req_header().uri.clone();
    let Some(rest) = uri.path().strip_prefix(PREFIX) else {
        return Ok(false);
    };
    let Some((name, encoded_key)) = rest.split_once('/') else {
        return reply(session, 404, "Not found").await;
    };
    let name = match decode(name) {
        Ok(name) => name,
        Err(()) => return reply(session, 400, "Invalid key").await,
    };
    let key = match encoded_key
        .split('/')
        .map(decode)
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(parts) if parts.iter().all(|s| !s.contains('/') && !s.contains('\\')) => parts.join("/"),
        _ => return reply(session, 400, "Invalid key").await,
    };
    if crate::validate_relative(&key).is_err() {
        return reply(session, 400, "Invalid key").await;
    }
    let Some(binding) = bindings
        .get(&name)
        .filter(|b| b.provider == StorageProvider::Local)
    else {
        return reply(session, 404, "Not found").await;
    };
    let method = session.req_header().method.as_str();
    if !matches!(method, "GET" | "PUT") {
        return reply(session, 405, "Method not allowed").await;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_secs();
    if !authorized(
        method,
        &name,
        encoded_key,
        uri.query().unwrap_or(""),
        binding.signing_key.as_deref().unwrap_or(""),
        now,
    ) {
        return reply(session, 403, "Forbidden").await;
    }
    if session
        .req_header()
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|size| size > MAX_BODY_BYTES)
    {
        return reply(session, 413, "Payload too large").await;
    }
    let binding = binding.clone();
    let root: PathBuf = root.into();
    let legacy_root: PathBuf = legacy_root.into();
    let store = tokio::task::spawn_blocking(move || {
        let relative = crate::required(&binding.path, "path")?;
        crate::validate_relative(relative)?;
        crate::migration::prepare(&root, &legacy_root, relative)?;
        crate::from_binding(&binding, &root)
    })
    .await
    .map_err(internal)?;
    let store = match store {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(%error, "Local storage unavailable");
            return reply(session, 503, "Storage unavailable").await;
        }
    };
    let key = match ObjectPath::parse(key) {
        Ok(key) => key,
        Err(_) => return reply(session, 400, "Invalid key").await,
    };
    if session.req_header().method == http::Method::PUT {
        let mut writer = BufWriter::with_capacity(Arc::clone(&store), key, 1024 * 1024);
        let mut received = 0_u64;
        loop {
            let chunk = match session.read_request_body().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    let _ = writer.abort().await;
                    return Err(error);
                }
            };
            received = received.saturating_add(chunk.len() as u64);
            if received > MAX_BODY_BYTES {
                let _ = writer.abort().await;
                return reply(session, 413, "Payload too large").await;
            }
            if let Err(error) = writer.put(chunk).await {
                let _ = writer.abort().await;
                tracing::error!(%error, "Local storage upload failed");
                return reply(session, 500, "Storage unavailable").await;
            }
        }
        if let Err(error) = writer.shutdown().await {
            tracing::error!(%error, "Local storage commit failed");
            return reply(session, 500, "Storage unavailable").await;
        }
        return reply(session, 204, "").await;
    }
    let result = match store.get(&key).await {
        Ok(result) => result,
        Err(object_store::Error::NotFound { .. }) => return reply(session, 404, "Not found").await,
        Err(error) => {
            tracing::error!(%error, "Local storage read failed");
            return reply(session, 500, "Storage unavailable").await;
        }
    };
    let mut header = ResponseHeader::build(200, None)?;
    header.insert_header("Content-Type", "application/octet-stream")?;
    header.insert_header("Content-Length", result.meta.size.to_string())?;
    header.insert_header("Cache-Control", "private, no-store")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    let mut chunks = result.into_stream();
    while let Some(chunk) = chunks.try_next().await.map_err(internal)? {
        session.write_response_body(Some(chunk), false).await?;
    }
    session.write_response_body(None, true).await?;
    Ok(true)
}

fn decode(value: &str) -> std::result::Result<String, ()> {
    let bytes = value.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'%'
            && (i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit())
        {
            return Err(());
        }
    }
    percent_decode_str(value)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| ())
}

fn authorized(method: &str, name: &str, key: &str, query: &str, secret: &str, now: u64) -> bool {
    if secret.is_empty() {
        return false;
    }
    let pairs: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).collect();
    let Some(expires) = pairs
        .get("expires")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|e| *e >= now && *e <= 9_007_199_254_740_991)
    else {
        return false;
    };
    let Some(token) = pairs
        .get("token")
        .and_then(|s| hex::decode(s.as_ref()).ok())
    else {
        return false;
    };
    let payload = format!("{method}\n{name}\n{key}\n{expires}");
    let signature = (|| {
        let key = PKey::hmac(secret.as_bytes())?;
        let mut signer = Signer::new(MessageDigest::sha256(), &key)?;
        signer.update(payload.as_bytes())?;
        signer.sign_to_vec()
    })();
    signature.is_ok_and(|s| s.len() == token.len() && openssl::memcmp::eq(&s, &token))
}

fn internal(error: impl std::fmt::Display) -> Box<Error> {
    Error::explain(ErrorType::InternalError, error.to_string())
}

async fn reply(session: &mut Session, status: u16, message: &str) -> Result<bool> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("Cache-Control", "no-store")?;
    if status == 204 {
        session
            .write_response_header(Box::new(header), true)
            .await?;
    } else {
        let body = serde_json::json!({"error": message}).to_string();
        header.insert_header("Content-Type", "application/json")?;
        header.insert_header("Content-Length", body.len().to_string())?;
        session
            .write_response_header(Box::new(header), false)
            .await?;
        session.write_response_body(Some(body.into()), true).await?;
    }
    Ok(true)
}
