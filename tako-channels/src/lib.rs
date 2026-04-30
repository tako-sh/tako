//! Shared channel infrastructure for Tako.
//!
//! Used by both `tako-server` (production) and the `tako dev` server.

pub mod close_codes;
pub mod error_codes;
pub mod params;
pub mod pattern;

pub use close_codes::ChannelCloseCode;

use parking_lot::Mutex;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHANNELS_DB_FILENAME: &str = "channels.sqlite3";
const DEFAULT_REPLAY_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
const DEFAULT_INACTIVITY_TTL_MS: u64 = 0;
const DEFAULT_KEEPALIVE_INTERVAL_MS: u64 = 25 * 1000;
const DEFAULT_MAX_CONNECTION_LIFETIME_MS: u64 = 2 * 60 * 60 * 1000;

pub const CHANNELS_BASE_PATH: &str = "/channels/";
pub const INTERNAL_CHANNEL_AUTH_PATH: &str = "/channels/authorize";
pub const INTERNAL_CHANNEL_DISPATCH_PATH: &str = "/channels/dispatch";

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelOperation {
    Publish,
    Subscribe,
    Connect,
}

impl ChannelOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Subscribe => "subscribe",
            Self::Connect => "connect",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEndpoint {
    Read,
    Messages,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRoute {
    pub channel: String,
    pub endpoint: ChannelEndpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelPublishPayload {
    pub r#type: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelMessage {
    pub id: String,
    pub channel: String,
    pub r#type: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelHeaderValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    pub value: String,
}

impl ChannelHeaderValue {
    pub fn parse(raw: &str) -> Self {
        if let Some(idx) = raw.find(' ') {
            Self {
                scheme: Some(raw[..idx].to_string()),
                value: raw[idx + 1..].to_string(),
            }
        } else {
            Self {
                scheme: None,
                value: raw.to_string(),
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelAuthVerifyRequest {
    pub channel: String,
    pub operation: String,
    pub params: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<ChannelHeaderValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelAuthRequest {
    pub channel: String,
    pub operation: String,
    pub request: ChannelAuthHttpRequest,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelAuthHttpRequest {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChannelTransport {
    Ws,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelAuthScheme {
    Required {
        header_name: Option<String>,
        cookie_name: Option<String>,
    },
    Public,
}

impl Serialize for ChannelAuthScheme {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Public => serializer.serialize_bool(false),
            Self::Required {
                header_name,
                cookie_name,
            } => {
                use serde::ser::SerializeMap;

                let len = usize::from(header_name.is_some()) + usize::from(cookie_name.is_some());
                let mut map = serializer.serialize_map(Some(len))?;
                if let Some(value) = header_name {
                    map.serialize_entry("headerName", value)?;
                }
                if let Some(value) = cookie_name {
                    map.serialize_entry("cookieName", value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ChannelAuthScheme {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Bool(false) => Ok(Self::Public),
            serde_json::Value::Object(map) => Ok(Self::Required {
                header_name: map
                    .get("headerName")
                    .and_then(|value| value.as_str())
                    .map(String::from),
                cookie_name: map
                    .get("cookieName")
                    .and_then(|value| value.as_str())
                    .map(String::from),
            }),
            _ => Err(serde::de::Error::custom("expected false or object")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelDefinitionMeta {
    pub channel: String,
    #[serde(rename = "paramsSchema")]
    pub params_schema: serde_json::Value,
    pub auth: ChannelAuthScheme,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<ChannelTransport>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ChannelAuthResponse {
    pub ok: bool,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub transport: Option<ChannelTransport>,
    #[serde(default = "default_replay_window_ms", rename = "replayWindowMs")]
    pub replay_window_ms: u64,
    #[serde(default = "default_inactivity_ttl_ms", rename = "inactivityTtlMs")]
    pub inactivity_ttl_ms: u64,
    #[serde(
        default = "default_keepalive_interval_ms",
        rename = "keepaliveIntervalMs"
    )]
    pub keepalive_interval_ms: u64,
    #[serde(
        default = "default_max_connection_lifetime_ms",
        rename = "maxConnectionLifetimeMs"
    )]
    pub max_connection_lifetime_ms: u64,
}

fn default_replay_window_ms() -> u64 {
    DEFAULT_REPLAY_WINDOW_MS
}
fn default_inactivity_ttl_ms() -> u64 {
    DEFAULT_INACTIVITY_TTL_MS
}
fn default_keepalive_interval_ms() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL_MS
}
fn default_max_connection_lifetime_ms() -> u64 {
    DEFAULT_MAX_CONNECTION_LIFETIME_MS
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("invalid channel path")]
    InvalidPath,
    #[error("unsupported channel operation")]
    Unsupported,
    #[error("forbidden")]
    Forbidden,
    #[error("channel not defined")]
    NotDefined,
    #[error("requested channel cursor is outside the replay window")]
    StaleCursor,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("channel auth unavailable")]
    AuthUnavailable,
    #[error("storage error: {0}")]
    Storage(String),
}

// ── Parsing ──────────────────────────────────────────────────────────────────

pub fn parse_channel_route(path: &str) -> Result<Option<ChannelRoute>, ChannelError> {
    if !path.starts_with(CHANNELS_BASE_PATH) {
        return Ok(None);
    }

    let rest = &path[CHANNELS_BASE_PATH.len()..];
    if rest.is_empty() {
        return Err(ChannelError::InvalidPath);
    }

    let (raw_channel, endpoint) = if let Some(stripped) = rest.strip_suffix("/messages") {
        (stripped, ChannelEndpoint::Messages)
    } else {
        (rest, ChannelEndpoint::Read)
    };

    if raw_channel.is_empty() || raw_channel.starts_with('/') || raw_channel.ends_with('/') {
        return Err(ChannelError::InvalidPath);
    }
    if raw_channel.split('/').any(|seg| seg.is_empty()) {
        return Err(ChannelError::InvalidPath);
    }
    let channel = percent_decode_str(raw_channel)
        .decode_utf8()
        .map_err(|_| ChannelError::InvalidPath)?
        .into_owned();

    Ok(Some(ChannelRoute { channel, endpoint }))
}

pub fn parse_message_id_cursor(
    value: Option<&str>,
    field_name: &str,
) -> Result<Option<i64>, ChannelError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    value
        .parse::<i64>()
        .map(Some)
        .map_err(|_| ChannelError::BadRequest(format!("invalid '{field_name}' cursor")))
}

pub fn parse_ws_last_message_id(query: Option<&str>) -> Result<Option<i64>, ChannelError> {
    let Some(query) = query else {
        return Ok(None);
    };

    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or_default();
        if key != "last_message_id" {
            continue;
        }
        let value = percent_decode_str(parts.next().unwrap_or_default())
            .decode_utf8()
            .map_err(|_| ChannelError::BadRequest("invalid query encoding".to_string()))?;
        return parse_message_id_cursor(Some(value.as_ref()), "last_message_id");
    }

    Ok(None)
}

// ── DB path helper ───────────────────────────────────────────────────────────

/// Build the SQLite DB path from a data directory and app name.
/// Callers provide their own path resolution — production uses
/// `app_runtime_data_paths`, dev uses a local `.tako/dev/` layout.
pub fn channels_db_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join(CHANNELS_DB_FILENAME)
}

// ── Auth ─────────────────────────────────────────────────────────────────────

/// Authorize a channel operation by calling the app's internal endpoint.
///
/// `endpoint` is the app's `host:port` (e.g. `127.0.0.1:3000`).
/// `internal_host` is the Host header for internal requests (e.g. `tako.internal`).
/// `internal_token` is the shared secret for the internal token header.
#[allow(clippy::too_many_arguments)]
pub async fn authorize_channel_request(
    endpoint: &str,
    internal_host: &str,
    internal_token_header: &str,
    internal_token: &str,
    operation: ChannelOperation,
    channel: &str,
    request_url: String,
    request_method: &str,
    request_headers: HashMap<String, String>,
) -> Result<ChannelAuthResponse, ChannelError> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| ChannelError::Storage(format!("build auth client: {e}")))?;

    let response = client
        .post(format!("http://{endpoint}{INTERNAL_CHANNEL_AUTH_PATH}"))
        .header("Host", internal_host)
        .header(internal_token_header, internal_token)
        .json(&ChannelAuthRequest {
            channel: channel.to_string(),
            operation: operation.as_str().to_string(),
            request: ChannelAuthHttpRequest {
                url: request_url,
                method: Some(request_method.to_string()),
                headers: request_headers,
            },
        })
        .send()
        .await
        .map_err(|_| ChannelError::AuthUnavailable)?;

    match response.status().as_u16() {
        200 => response
            .json::<ChannelAuthResponse>()
            .await
            .map_err(|e| ChannelError::BadRequest(format!("invalid auth response: {e}"))),
        403 => Err(ChannelError::Forbidden),
        404 => Err(ChannelError::NotDefined),
        405 => Ok(ChannelAuthResponse {
            ok: false,
            subject: None,
            transport: None,
            replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
            inactivity_ttl_ms: DEFAULT_INACTIVITY_TTL_MS,
            keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
            max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
        }),
        _ => Err(ChannelError::AuthUnavailable),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelDispatchRequest {
    pub channel: String,
    pub frame: ChannelPublishPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ChannelDispatchResponse {
    Fanout {
        data: serde_json::Value,
    },
    Drop {
        #[serde(default)]
        error: Option<String>,
    },
    Reject {
        reason: String,
    },
}

/// Dispatch a client-initiated WS frame through the app's declared
/// per-channel handler. Returns the action to take: fanout the returned
/// data, drop the message, or reject (reason-coded) the connection.
pub async fn dispatch_channel_message(
    endpoint: &str,
    internal_host: &str,
    internal_token_header: &str,
    internal_token: &str,
    request: ChannelDispatchRequest,
) -> Result<ChannelDispatchResponse, ChannelError> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| ChannelError::Storage(format!("build dispatch client: {e}")))?;

    let response = client
        .post(format!("http://{endpoint}{INTERNAL_CHANNEL_DISPATCH_PATH}"))
        .header("Host", internal_host)
        .header(internal_token_header, internal_token)
        .json(&request)
        .send()
        .await
        .map_err(|_| ChannelError::AuthUnavailable)?;

    match response.status().as_u16() {
        200 => response
            .json::<ChannelDispatchResponse>()
            .await
            .map_err(|e| ChannelError::BadRequest(format!("invalid dispatch response: {e}"))),
        403 => Err(ChannelError::Forbidden),
        404 => Err(ChannelError::NotDefined),
        _ => Err(ChannelError::AuthUnavailable),
    }
}

// ── Store ────────────────────────────────────────────────────────────────────

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Per-app SQLite-backed channel store.
///
/// The connection is opened once and reused; every operation locks a
/// mutex and uses the cached connection. Callers should hold a single
/// `ChannelStore` for each DB path and share it across requests (e.g.
/// behind an `Arc`) — constructing a new `ChannelStore` reruns pragmas
/// and schema init on every call.
pub struct ChannelStore {
    conn: Mutex<rusqlite::Connection>,
}

impl ChannelStore {
    /// Open (or create) the channel DB at `path` and run the idempotent
    /// schema init. Safe to call repeatedly against the same path — SQLite
    /// supports multiple connections per file — but callers are expected to
    /// hold the returned store for the process's lifetime rather than
    /// reopening per request.
    pub fn open(path: &Path) -> Result<Self, ChannelError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ChannelError::Storage(format!("create channel dir: {e}")))?;
        }
        let conn =
            rusqlite::Connection::open(path).map_err(|e| ChannelError::Storage(e.to_string()))?;
        init_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE channel_metadata SET last_activity_unix_ms = ?2 WHERE channel = ?1",
            rusqlite::params![channel, now_unix_ms()],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT INTO channel_messages (channel, type, data_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                channel,
                payload.r#type,
                serde_json::to_string(&payload.data)
                    .map_err(|e| ChannelError::BadRequest(format!("serialize payload: {e}")))?,
            ],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

        let id = conn.last_insert_rowid();
        Ok(ChannelMessage {
            id: id.to_string(),
            channel: channel.to_string(),
            r#type: payload.r#type.clone(),
            data: payload.data.clone(),
        })
    }

    pub fn read_after(
        &self,
        channel: &str,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, channel, type, data_json
                 FROM channel_messages
                 WHERE channel = ?1 AND (?2 IS NULL OR id > ?2)
                 ORDER BY id ASC
                 LIMIT ?3",
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![channel, after, i64::from(limit)], |row| {
                let data_json: String = row.get(3)?;
                let data = serde_json::from_str(&data_json).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?;
                Ok(ChannelMessage {
                    id: row.get::<_, i64>(0)?.to_string(),
                    channel: row.get(1)?,
                    r#type: row.get(2)?,
                    data,
                })
            })
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| ChannelError::Storage(e.to_string()))
    }

    pub fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<i64>,
    ) -> Result<Option<i64>, ChannelError> {
        let conn = self.conn.lock();
        let latest = message_id(&conn, channel, "MAX")?;
        let Some(requested) = requested else {
            return Ok(latest);
        };

        let Some(oldest) = message_id(&conn, channel, "MIN")? else {
            return Ok(Some(requested));
        };

        if requested < oldest.saturating_sub(1) {
            return Err(ChannelError::StaleCursor);
        }

        Ok(Some(requested))
    }

    pub fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        let conn = self.conn.lock();
        let now = now_unix_ms();
        conn.execute(
            "INSERT INTO channel_metadata (
                channel,
                replay_window_ms,
                inactivity_ttl_ms,
                keepalive_interval_ms,
                max_connection_lifetime_ms,
                last_activity_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(channel) DO UPDATE SET
                replay_window_ms = excluded.replay_window_ms,
                inactivity_ttl_ms = excluded.inactivity_ttl_ms,
                keepalive_interval_ms = excluded.keepalive_interval_ms,
                max_connection_lifetime_ms = excluded.max_connection_lifetime_ms,
                last_activity_unix_ms = excluded.last_activity_unix_ms",
            rusqlite::params![
                channel,
                auth.replay_window_ms as i64,
                auth.inactivity_ttl_ms as i64,
                auth.keepalive_interval_ms as i64,
                auth.max_connection_lifetime_ms as i64,
                now,
            ],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

        if auth.replay_window_ms > 0 {
            let cutoff = now - auth.replay_window_ms as i64;
            conn.execute(
                "DELETE FROM channel_messages WHERE channel = ?1 AND created_at_unix_ms < ?2",
                rusqlite::params![channel, cutoff],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        }

        conn.execute(
            "DELETE FROM channel_messages
             WHERE channel IN (
                SELECT channel
                FROM channel_metadata
                WHERE inactivity_ttl_ms > 0
                  AND last_activity_unix_ms < (?1 - inactivity_ttl_ms)
             )",
            rusqlite::params![now],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        conn.execute(
            "DELETE FROM channel_metadata
             WHERE inactivity_ttl_ms > 0
               AND last_activity_unix_ms < (?1 - inactivity_ttl_ms)",
            rusqlite::params![now],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

        Ok(())
    }
}

fn message_id(
    conn: &rusqlite::Connection,
    channel: &str,
    aggregate: &str,
) -> Result<Option<i64>, ChannelError> {
    let sql = format!("SELECT {aggregate}(id) FROM channel_messages WHERE channel = ?1");
    conn.query_row(&sql, rusqlite::params![channel], |row| row.get(0))
        .map_err(|e| ChannelError::Storage(e.to_string()))
}

fn init_connection(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS channel_messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             channel TEXT NOT NULL,
             type TEXT NOT NULL,
             data_json TEXT NOT NULL,
             created_at_unix_ms INTEGER NOT NULL DEFAULT (unixepoch() * 1000)
         );",
    )
    .map_err(|e| ChannelError::Storage(e.to_string()))?;

    ensure_channel_metadata_schema(conn)?;

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_channel_messages_channel_id
         ON channel_messages(channel, id);",
    )
    .map_err(|e| ChannelError::Storage(e.to_string()))?;
    Ok(())
}

fn ensure_channel_metadata_schema(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'channel_metadata'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

    if exists == 0 {
        conn.execute_batch(
            "CREATE TABLE channel_metadata (
                channel TEXT PRIMARY KEY,
                replay_window_ms INTEGER NOT NULL,
                inactivity_ttl_ms INTEGER NOT NULL,
                keepalive_interval_ms INTEGER NOT NULL,
                max_connection_lifetime_ms INTEGER NOT NULL,
                last_activity_unix_ms INTEGER NOT NULL
            );",
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        return Ok(());
    }

    let mut columns = conn
        .prepare("PRAGMA table_info(channel_metadata)")
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    let columns = columns
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| ChannelError::Storage(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ChannelError::Storage(e.to_string()))?;

    if columns.iter().any(|column| column == "retention_ms")
        && !columns.iter().any(|column| column == "replay_window_ms")
    {
        conn.execute_batch(
            "ALTER TABLE channel_metadata RENAME COLUMN retention_ms TO replay_window_ms;",
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_channel_route_rejects_invalid_paths() {
        assert!(matches!(
            parse_channel_route("/channels/"),
            Err(ChannelError::InvalidPath)
        ));
        assert!(matches!(
            parse_channel_route("/channels//messages"),
            Err(ChannelError::InvalidPath)
        ));
    }

    #[test]
    fn parse_channel_route_accepts_multi_segment_names() {
        let route = parse_channel_route("/channels/chat/abc-123")
            .unwrap()
            .unwrap();
        assert_eq!(route.channel, "chat/abc-123");
        assert_eq!(route.endpoint, ChannelEndpoint::Read);

        let route = parse_channel_route("/channels/chat/abc-123/messages")
            .unwrap()
            .unwrap();
        assert_eq!(route.channel, "chat/abc-123");
        assert_eq!(route.endpoint, ChannelEndpoint::Messages);
    }

    #[test]
    fn parse_channel_route_decodes_percent_encoded_segment() {
        let route = parse_channel_route("/channels/chat%3Aroom-123")
            .unwrap()
            .unwrap();
        assert_eq!(route.channel, "chat:room-123");
        assert_eq!(route.endpoint, ChannelEndpoint::Read);
    }

    #[test]
    fn parse_ws_last_message_id_reads_cursor() {
        let after = parse_ws_last_message_id(Some("last_message_id=42&noop=1")).unwrap();
        assert_eq!(after, Some(42));
    }

    #[test]
    fn header_value_splits_on_first_space() {
        assert_eq!(
            ChannelHeaderValue::parse("Bearer abc 123"),
            ChannelHeaderValue {
                scheme: Some("Bearer".to_string()),
                value: "abc 123".to_string(),
            },
        );
        assert_eq!(
            ChannelHeaderValue::parse("plain-token"),
            ChannelHeaderValue {
                scheme: None,
                value: "plain-token".to_string(),
            },
        );
    }

    #[test]
    fn verify_request_serializes_with_optional_credentials() {
        let req = ChannelAuthVerifyRequest {
            channel: "chat".to_string(),
            operation: "subscribe".to_string(),
            params: serde_json::json!({ "roomId": "room-9" }),
            header: Some(ChannelHeaderValue {
                scheme: Some("Bearer".to_string()),
                value: "abc123".to_string(),
            }),
            cookie: None,
        };

        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["channel"], "chat");
        assert_eq!(value["params"]["roomId"], "room-9");
        assert_eq!(value["header"]["scheme"], "Bearer");
        assert!(value.get("cookie").is_none());
    }

    #[test]
    fn auth_scheme_serializes_false_for_public() {
        let public = ChannelAuthScheme::Public;
        assert_eq!(
            serde_json::to_value(&public).unwrap(),
            serde_json::json!(false)
        );

        let header_only = ChannelAuthScheme::Required {
            header_name: Some("authorization".into()),
            cookie_name: None,
        };
        let value = serde_json::to_value(&header_only).unwrap();
        assert_eq!(value["headerName"], "authorization");
        assert!(value.get("cookieName").is_none());
    }

    #[test]
    fn auth_scheme_deserializes_false_or_object() {
        assert_eq!(
            serde_json::from_value::<ChannelAuthScheme>(serde_json::json!(false)).unwrap(),
            ChannelAuthScheme::Public,
        );
        assert_eq!(
            serde_json::from_value::<ChannelAuthScheme>(serde_json::json!({
                "headerName": "authorization",
                "cookieName": "sid"
            }))
            .unwrap(),
            ChannelAuthScheme::Required {
                header_name: Some("authorization".into()),
                cookie_name: Some("sid".into()),
            },
        );
    }

    #[test]
    fn channel_def_meta_serializes_params_schema_inline() {
        let meta = ChannelDefinitionMeta {
            channel: "chat".into(),
            params_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "roomId": { "type": "string" }
                }
            }),
            auth: ChannelAuthScheme::Required {
                header_name: Some("authorization".into()),
                cookie_name: None,
            },
            transport: Some(ChannelTransport::Ws),
        };

        let value = serde_json::to_value(&meta).unwrap();
        assert_eq!(value["channel"], "chat");
        assert_eq!(value["paramsSchema"]["type"], "object");
        assert_eq!(value["transport"], "ws");
    }

    #[test]
    fn channel_store_appends_and_reads_messages() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = ChannelStore::open(&temp.path().join("channels.sqlite3")).unwrap();

        let first = store
            .append(
                "chat:room-123",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({ "text": "hi" }),
                },
            )
            .unwrap();
        let second = store
            .append(
                "chat:room-123",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({ "text": "there" }),
                },
            )
            .unwrap();

        assert_eq!(first.id, "1");
        assert_eq!(second.id, "2");

        let messages = store.read_after("chat:room-123", Some(1), 100).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "2");
        assert_eq!(messages[0].data, serde_json::json!({ "text": "there" }));
    }

    #[test]
    fn channel_store_defaults_missing_cursor_to_latest_message() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = ChannelStore::open(&temp.path().join("channels.sqlite3")).unwrap();

        store
            .append(
                "chat:room-123",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({ "text": "hi" }),
                },
            )
            .unwrap();
        store
            .append(
                "chat:room-123",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({ "text": "there" }),
                },
            )
            .unwrap();

        assert_eq!(store.replay_cursor("chat:room-123", None).unwrap(), Some(2));
    }

    #[test]
    fn channel_store_rejects_stale_cursors() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("channels.sqlite3");
        let store = ChannelStore::open(&db_path).unwrap();

        store
            .append(
                "chat:room-123",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({ "text": "hi" }),
                },
            )
            .unwrap();
        store
            .append(
                "chat:room-123",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({ "text": "there" }),
                },
            )
            .unwrap();
        store
            .conn
            .lock()
            .execute("DELETE FROM channel_messages WHERE id = 1", [])
            .unwrap();

        assert!(matches!(
            store.replay_cursor("chat:room-123", Some(0)),
            Err(ChannelError::StaleCursor)
        ));
    }

    #[test]
    fn channel_store_persists_lifecycle_and_prunes_inactive_channels() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = ChannelStore::open(&temp.path().join("channels.sqlite3")).unwrap();

        store
            .sync_channel(
                "chat:room-123",
                &ChannelAuthResponse {
                    ok: true,
                    subject: None,
                    transport: None,
                    replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
                    inactivity_ttl_ms: 1,
                    keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
                    max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
                },
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store
            .sync_channel(
                "chat:room-456",
                &ChannelAuthResponse {
                    ok: true,
                    subject: None,
                    transport: Some(ChannelTransport::Ws),
                    replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
                    inactivity_ttl_ms: 0,
                    keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
                    max_connection_lifetime_ms: DEFAULT_MAX_CONNECTION_LIFETIME_MS,
                },
            )
            .unwrap();

        let conn = store.conn.lock();
        let channels = conn
            .prepare("SELECT channel FROM channel_metadata ORDER BY channel ASC")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(channels, vec!["chat:room-456".to_string()]);
    }

    #[test]
    fn channel_store_reopen_preserves_existing_messages() {
        // Guards the invariant that data persists to disk: opening the same
        // path again (e.g. after a process restart) must see the prior rows.
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("channels.sqlite3");

        {
            let store = ChannelStore::open(&db_path).unwrap();
            store
                .append(
                    "chat:room-123",
                    &ChannelPublishPayload {
                        r#type: "message".to_string(),
                        data: serde_json::json!({ "text": "hi" }),
                    },
                )
                .unwrap();
        }

        let reopened = ChannelStore::open(&db_path).unwrap();
        let messages = reopened.read_after("chat:room-123", None, 100).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data, serde_json::json!({ "text": "hi" }));
    }
}
