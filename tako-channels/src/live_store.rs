//! Read-only Store subscriptions built on the channel transport.

use crate::{
    ChannelAuthResponse, ChannelAuthScheme, ChannelDefinitionMeta, ChannelError,
    ChannelHeaderValue, ChannelOperation, ChannelPublishPayload, ChannelStore,
};
use bytes::Bytes;
use percent_encoding::percent_decode_str;
use pingora_core::prelude::*;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const STORE_CHANNEL_PREFIX: &str = "tako.store:";
pub const STORES_BASE_PATH: &str = "/_tako/stores/";
pub const INTERNAL_STORE_LOAD_PATH: &str = "/stores/load";
pub const STORE_READY_JSON: &str = r#"{"type":"tako.store.ready","data":{}}"#;
pub const MAX_STORE_RESPONSE_BYTES: usize = 1 << 20;
pub const MAX_STORE_PARAMS_BYTES: usize = 16 << 10;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub fn validate_store_lifecycle(
    auth: &ChannelAuthResponse,
) -> std::result::Result<(), ChannelError> {
    let now = tokio::time::Instant::now();
    for milliseconds in [auth.keepalive_interval_ms, auth.max_connection_lifetime_ms] {
        if milliseconds == 0
            || milliseconds > MAX_SAFE_INTEGER
            || now
                .checked_add(Duration::from_millis(milliseconds))
                .is_none()
        {
            return Err(ChannelError::AuthUnavailable);
        }
    }
    Ok(())
}

pub fn validate_store_params(params: &Value) -> std::result::Result<(), ChannelError> {
    if !params.is_object()
        || serde_json::to_vec(params).map_or(true, |bytes| bytes.len() > MAX_STORE_PARAMS_BYTES)
    {
        return Err(ChannelError::BadRequest(
            "Store params must be an object of at most 16 KiB".into(),
        ));
    }
    Ok(())
}

/// Stores carry a typed JSON object in one query field. Unlike ordinary
/// channel URL params this preserves unions, null, arrays, and nested values
/// without inferring their types from strings.
pub fn validate_store_query(
    schema: &Value,
    query: &str,
) -> std::result::Result<Value, ChannelError> {
    let params = if query.is_empty() {
        json!({})
    } else {
        if query.len() > MAX_STORE_PARAMS_BYTES * 3 + 32 || query.contains('&') {
            return Err(ChannelError::BadRequest(
                "Expected one Store params field".into(),
            ));
        }
        let (key, raw) = query
            .split_once('=')
            .ok_or_else(|| ChannelError::BadRequest("Expected a Store params field".into()))?;
        let key = key.replace('+', " ");
        let key = percent_decode_str(&key)
            .decode_utf8()
            .map_err(|_| ChannelError::BadRequest("Invalid Store query encoding".into()))?;
        if key != "params" {
            return Err(ChannelError::BadRequest("Unknown Store query field".into()));
        }
        let raw = raw.replace('+', " ");
        let raw = percent_decode_str(&raw)
            .decode_utf8()
            .map_err(|_| ChannelError::BadRequest("Invalid Store params encoding".into()))?;
        if raw.len() > MAX_STORE_PARAMS_BYTES {
            return Err(ChannelError::BadRequest(
                "Store params exceed 16 KiB".into(),
            ));
        }
        serde_json::from_str(&raw)
            .map_err(|_| ChannelError::BadRequest("Invalid Store params JSON".into()))?
    };
    validate_store_params(&params)?;
    let validator =
        jsonschema::draft202012::new(schema).map_err(|_| ChannelError::AuthUnavailable)?;
    validator
        .validate(&params)
        .map_err(|_| ChannelError::BadRequest("Invalid Store params".into()))?;
    Ok(params)
}

pub fn valid_store_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub fn parse_store_route(path: &str) -> std::result::Result<Option<String>, ChannelError> {
    let Some(name) = path.strip_prefix(STORES_BASE_PATH) else {
        return Ok(None);
    };
    if name.is_empty() || name.contains('/') {
        return Err(ChannelError::InvalidPath);
    }
    let name = percent_decode_str(name)
        .decode_utf8()
        .map_err(|_| ChannelError::InvalidPath)?;
    if !valid_store_name(&name) {
        return Err(ChannelError::InvalidPath);
    }
    Ok(Some(name.into_owned()))
}

/// The registry and verifier use the base name; storage uses this opaque,
/// bounded key. Numeric query coercion may produce 1.0 where an SDK sends 1.
pub fn store_scope(channel: &str, params: &Value) -> String {
    fn normalize(value: &mut Value) {
        match value {
            Value::Object(object) => object.values_mut().for_each(normalize),
            Value::Array(values) => values.iter_mut().for_each(normalize),
            Value::Number(number) if number.is_f64() => {
                let value = number.as_f64().expect("JSON numbers are finite");
                if value.fract() == 0.0 {
                    if value >= i64::MIN as f64 && value < i64::MAX as f64 {
                        *number = (value as i64).into();
                    } else if value >= 0.0 && value < u64::MAX as f64 {
                        *number = (value as u64).into();
                    }
                }
            }
            _ => {}
        }
    }
    let mut params = params.clone();
    normalize(&mut params);
    format!(
        "{STORE_CHANNEL_PREFIX}{}",
        crate::presence_scope(channel, &params)
    )
}

/// Validate the trusted socket publication before choosing its storage key.
/// Scope params are never included in the notification sent to subscribers.
pub fn publication_channel(
    channel: &str,
    payload: &Value,
) -> std::result::Result<String, ChannelError> {
    if !channel.starts_with(STORE_CHANNEL_PREFIX) {
        return Ok(channel.into());
    }
    if !valid_store_name(&channel[STORE_CHANNEL_PREFIX.len()..])
        || payload.get("type").and_then(Value::as_str) != Some("tako.store.invalidate")
        || !payload
            .get("data")
            .and_then(Value::as_object)
            .is_some_and(|data| data.is_empty())
    {
        return Err(ChannelError::BadRequest(
            "Invalid Store invalidation".into(),
        ));
    }
    let params = payload
        .get("scopeParams")
        .filter(|params| params.is_object())
        .ok_or_else(|| ChannelError::BadRequest("Store scopeParams must be an object".into()))?;
    validate_store_params(params)?;
    Ok(store_scope(channel, params))
}

fn store_http_client() -> std::result::Result<&'static reqwest::Client, ChannelError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ChannelError::AuthUnavailable)?;
    Ok(CLIENT.get_or_init(|| client))
}

/// Fetch trusted app metadata or a loader response without buffering an
/// unbounded response. Tokens and app error bodies never reach the browser.
pub async fn internal_store_json(
    endpoint: &str,
    internal_host: &str,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> std::result::Result<Value, ChannelError> {
    let client = store_http_client()?;
    let url = format!("http://{endpoint}{path}");
    let request = match body {
        Some(body) => client.post(url).json(body),
        None => client.get(url),
    };
    let mut response = request
        .header("Host", internal_host)
        .header("x-tako-internal-token", token)
        .send()
        .await
        .map_err(|_| ChannelError::AuthUnavailable)?;
    match response.status().as_u16() {
        200 => {}
        400 => return Err(ChannelError::BadRequest("Invalid Store request".into())),
        // Authentication here is the app bootstrap credential. User denials
        // are handled separately by /channels/authorize before this request.
        401 | 403 => return Err(ChannelError::AuthUnavailable),
        404 => return Err(ChannelError::NotDefined),
        _ => return Err(ChannelError::AuthUnavailable),
    }
    let valid_response_token = response
        .headers()
        .get("x-tako-internal-token")
        .and_then(|header| header.to_str().ok())
        .is_some_and(|echo| {
            echo.len() == token.len() && openssl::memcmp::eq(echo.as_bytes(), token.as_bytes())
        });
    if token.is_empty() || !valid_response_token {
        return Err(ChannelError::AuthUnavailable);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_STORE_RESPONSE_BYTES as u64)
    {
        return Err(ChannelError::AuthUnavailable);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ChannelError::AuthUnavailable)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_STORE_RESPONSE_BYTES {
            return Err(ChannelError::AuthUnavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ChannelError::AuthUnavailable)
}

pub async fn load_store(
    endpoint: &str,
    internal_host: &str,
    token: &str,
    name: &str,
    params: Value,
    subject: Option<&str>,
) -> std::result::Result<Value, ChannelError> {
    validate_store_params(&params)?;
    let mut body = json!({"store": name, "params": params});
    if let Some(subject) = subject {
        body["subject"] = Value::String(subject.into());
    }
    let response = internal_store_json(
        endpoint,
        internal_host,
        token,
        INTERNAL_STORE_LOAD_PATH,
        Some(&body),
    )
    .await?;
    if !response.is_object() || response.get("data").is_none() {
        return Err(ChannelError::AuthUnavailable);
    }
    if let Some(offline) = response.get("offline") {
        let valid_version = offline
            .get("version")
            .and_then(Value::as_str)
            .is_some_and(|version| !version.is_empty() && version.len() <= 128);
        let valid_age = offline
            .get("maxAgeMs")
            .and_then(Value::as_f64)
            .is_some_and(|age| age > 0.0 && age.fract() == 0.0 && age <= 9_007_199_254_740_991.0);
        if !valid_version || !valid_age {
            return Err(ChannelError::AuthUnavailable);
        }
    }
    Ok(response)
}

/// Local dev uses the same metadata, typed params, and authorization as production.
/// Ordinary dev channels retain their existing permissive behavior.
pub async fn authorize_store(
    endpoint: &str,
    internal_host: &str,
    token: &str,
    channel: &str,
    query: &str,
    headers: &HashMap<String, String>,
) -> std::result::Result<(Value, ChannelAuthResponse), ChannelError> {
    let metadata = internal_store_json(
        endpoint,
        internal_host,
        token,
        crate::INTERNAL_CHANNEL_REGISTRY_PATH,
        None,
    )
    .await?;
    let definitions: Vec<ChannelDefinitionMeta> =
        serde_json::from_value(metadata).map_err(|_| ChannelError::AuthUnavailable)?;
    let definition = definitions
        .into_iter()
        .find(|definition| definition.channel == channel)
        .ok_or(ChannelError::NotDefined)?;
    let params = validate_store_query(&definition.params_schema, query)?;
    let (header, cookie) = match &definition.auth {
        ChannelAuthScheme::Public => (None, None),
        ChannelAuthScheme::Required {
            header_name,
            cookie_name,
        } => {
            let header = header_name
                .as_ref()
                .and_then(|name| headers.get(&name.to_ascii_lowercase()))
                .map(|raw| ChannelHeaderValue::parse(raw));
            let cookie = cookie_name.as_ref().and_then(|name| {
                headers.get("cookie")?.split(';').find_map(|pair| {
                    let (key, value) = pair.trim().split_once('=')?;
                    (key == name).then(|| value.to_owned())
                })
            });
            if (header_name.is_some() || cookie_name.is_some())
                && header.is_none()
                && cookie.is_none()
            {
                return Err(ChannelError::Unauthorized);
            }
            (header, cookie)
        }
    };
    let auth = crate::authorize_channel_request(
        endpoint,
        internal_host,
        "x-tako-internal-token",
        token,
        ChannelOperation::Subscribe,
        channel,
        params.clone(),
        header,
        cookie,
    )
    .await?;
    validate_store_lifecycle(&auth)?;
    Ok((params, auth))
}

/// Start from the current head on every connection. The ready message is sent
/// only after capturing the cursor and subscribing to changes, so a load begun
/// by the client after ready cannot miss a concurrent invalidation.
pub async fn serve_store_events(
    session: &mut Session,
    store: &Arc<ChannelStore>,
    stream: &str,
    auth: &ChannelAuthResponse,
) -> Result<bool> {
    validate_store_lifecycle(auth).map_err(store_error)?;
    store
        .sync_channel_async(stream, auth)
        .await
        .map_err(store_error)?;
    let mut changes = store.changes();
    let mut after = store
        .replay_cursor_async(stream, None)
        .await
        .map_err(store_error)?;
    let started = tokio::time::Instant::now();
    let keepalive = Duration::from_millis(auth.keepalive_interval_ms);
    let deadline = started
        .checked_add(Duration::from_millis(auth.max_connection_lifetime_ms))
        .ok_or_else(|| store_error(ChannelError::AuthUnavailable))?;
    let mut next_keepalive = started
        .checked_add(keepalive)
        .ok_or_else(|| store_error(ChannelError::AuthUnavailable))?;
    let mut header = ResponseHeader::build(200, None)?;
    header.insert_header("Content-Type", "text/event-stream")?;
    header.insert_header("Cache-Control", "no-store")?;
    header.insert_header("Connection", "keep-alive")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(
            Some(Bytes::from(format!("data: {STORE_READY_JSON}\n\n"))),
            false,
        )
        .await?;

    loop {
        changes.borrow_and_update();
        let messages = store
            .read_after_async(stream, after, 100)
            .await
            .map_err(store_error)?;
        let full = messages.len() == 100;
        for message in messages {
            after = message.id.parse().ok();
            // Only the native Store contract crosses this stream, even if an
            // older process wrote other channel messages under the same key.
            let payload = ChannelPublishPayload {
                r#type: "tako.store.invalidate".into(),
                data: json!({}),
            };
            let frame = format!(
                "data: {}\n\n",
                serde_json::to_string(&payload).expect("Store notification serializes")
            );
            session
                .write_response_body(Some(Bytes::from(frame)), false)
                .await?;
        }
        if tokio::time::Instant::now() >= deadline {
            session.write_response_body(None, true).await?;
            return Ok(true);
        }
        if tokio::time::Instant::now() >= next_keepalive {
            session
                .write_response_body(Some(Bytes::from_static(b": keepalive\n\n")), false)
                .await?;
            next_keepalive = tokio::time::Instant::now()
                .checked_add(keepalive)
                .unwrap_or(deadline);
        }
        if full {
            continue;
        }
        tokio::select! {
            _ = changes.changed() => {},
            _ = tokio::time::sleep_until(next_keepalive.min(deadline)) => {},
        }
    }
}

fn store_error(error: ChannelError) -> Box<pingora_core::Error> {
    Error::explain(ErrorType::InternalError, error.to_string())
}

#[cfg(test)]
mod tests;
