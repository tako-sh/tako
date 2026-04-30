//! Channel support for `tako dev`, backed by `tako-channels` (SQLite).
//!
//! No auth in dev mode — all operations are allowed. Production uses
//! `tako-server/src/proxy/service.rs` with per-request authorization.

use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tako_channels::{
    ChannelAuthResponse, ChannelPublishPayload, ChannelStore, parse_channel_route,
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Default auth response for dev mode — permissive, no subject, SSE transport.
fn dev_auth_response() -> ChannelAuthResponse {
    ChannelAuthResponse {
        ok: true,
        subject: None,
        transport: None,
        replay_window_ms: 24 * 60 * 60 * 1000,
        inactivity_ttl_ms: 0,
        keepalive_interval_ms: 25_000,
        max_connection_lifetime_ms: 2 * 60 * 60 * 1000,
    }
}

#[derive(Clone)]
pub struct DevChannelStore {
    store: Arc<ChannelStore>,
}

impl DevChannelStore {
    pub fn new(db_path: PathBuf) -> Self {
        let store = ChannelStore::open(&db_path).unwrap_or_else(|e| {
            panic!(
                "failed to open dev channel store at {}: {e}",
                db_path.display()
            )
        });
        Self {
            store: Arc::new(store),
        }
    }

    /// Append a message to `channel` and return the stored record. Used
    /// from the internal socket's `Command::ChannelPublish` path (server-side
    /// channel `.publish()` from app/workflow code).
    pub fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<tako_channels::ChannelMessage, tako_channels::ChannelError> {
        self.store.sync_channel(channel, &dev_auth_response())?;
        self.store.append(channel, payload)
    }
}

/// Try to handle a channel request. Returns `Ok(true)` if handled,
/// `Ok(false)` if the path is not a channel route (pass to upstream).
pub async fn try_handle(
    session: &mut Session,
    dev_store: &DevChannelStore,
    path: &str,
    method: &str,
) -> Result<bool> {
    let route = match parse_channel_route(path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(false),
        Err(_) => return write_json(session, 400, r#"{"error":"Invalid channel path"}"#).await,
    };

    if method != "GET" {
        return write_json(session, 405, r#"{"error":"Method not allowed"}"#).await;
    }
    serve_sse(session, dev_store, &route.channel).await
}

async fn serve_sse(
    session: &mut Session,
    dev_store: &DevChannelStore,
    channel: &str,
) -> Result<bool> {
    let store = &dev_store.store;
    let auth = dev_auth_response();

    // Sync channel metadata (creates the channel if needed, prunes stale data).
    if let Err(e) = store.sync_channel(channel, &auth) {
        let msg = format!(r#"{{"error":"Channel sync failed: {e}"}}"#);
        return write_json(session, 500, &msg).await;
    }

    // Resolve the initial cursor — start from the latest message so new
    // subscribers only see future events (matching production behavior).
    let mut after = match store.replay_cursor(channel, None) {
        Ok(cursor) => cursor,
        Err(e) => {
            let msg = format!(r#"{{"error":"Failed to resolve cursor: {e}"}}"#);
            return write_json(session, 500, &msg).await;
        }
    };

    let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
    let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));

    let mut header = ResponseHeader::build(200, None)?;
    header.insert_header("Content-Type", "text/event-stream")?;
    header.insert_header("Cache-Control", "no-store")?;
    header.insert_header("Connection", "keep-alive")?;
    header.insert_header("Access-Control-Allow-Origin", "*")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;

    let started = tokio::time::Instant::now();
    let mut next_keepalive = started + keepalive_interval;

    loop {
        match store.read_after(channel, after, 100) {
            Ok(messages) => {
                if !messages.is_empty() {
                    for message in messages {
                        let encoded = serde_json::to_string(&message).unwrap_or_default();
                        let frame = format!("id: {}\ndata: {}\n\n", message.id, encoded);
                        if session
                            .write_response_body(Some(frame.into_bytes().into()), false)
                            .await
                            .is_err()
                        {
                            return Ok(true);
                        }
                        after = Some(
                            message
                                .id
                                .parse::<i64>()
                                .expect("channel ids are always numeric"),
                        );
                    }
                    next_keepalive = tokio::time::Instant::now() + keepalive_interval;
                }
            }
            Err(_) => {
                // Transient read error — skip this poll cycle.
            }
        }

        if started.elapsed() >= max_connection_lifetime {
            session.write_response_body(None, true).await?;
            return Ok(true);
        }

        let now = tokio::time::Instant::now();
        if now >= next_keepalive {
            if session
                .write_response_body(
                    Some(": keepalive\n\n".to_string().into_bytes().into()),
                    false,
                )
                .await
                .is_err()
            {
                return Ok(true);
            }
            next_keepalive = now + keepalive_interval;
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn write_json(session: &mut Session, status: u16, body: &str) -> Result<bool> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("Content-Type", "application/json")?;
    header.insert_header("Access-Control-Allow-Origin", "*")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(body.to_string().into_bytes().into()), true)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tako_channels::parse_channel_route;

    #[test]
    fn parse_subscribe_path() {
        let route = parse_channel_route("/channels/demo-broadcast")
            .unwrap()
            .unwrap();
        assert_eq!(route.channel, "demo-broadcast");
    }

    #[test]
    fn parse_rejects_nested_channel_path() {
        assert!(parse_channel_route("/channels/demo-broadcast/messages").is_err());
    }

    #[test]
    fn parse_returns_none_for_non_channel_path() {
        assert!(parse_channel_route("/api/hello").unwrap().is_none());
    }

    #[test]
    fn parse_returns_error_for_empty_channel_path() {
        assert!(parse_channel_route("/channels/").is_err());
    }

    #[test]
    fn publish_and_read_via_store() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("channels.sqlite3");
        let dev_store = DevChannelStore::new(db_path);
        let store = &dev_store.store;

        // Sync the channel first.
        store.sync_channel("test", &dev_auth_response()).unwrap();

        let msg = store
            .append(
                "test",
                &ChannelPublishPayload {
                    r#type: "message".to_string(),
                    data: serde_json::json!({"text": "hello"}),
                },
            )
            .unwrap();

        assert_eq!(msg.id, "1");
        assert_eq!(msg.channel, "test");

        let messages = store.read_after("test", None, 100).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data, serde_json::json!({"text": "hello"}));
    }

    #[test]
    fn publish_increments_ids() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("channels.sqlite3");
        let dev_store = DevChannelStore::new(db_path);
        let store = &dev_store.store;

        store.sync_channel("ch", &dev_auth_response()).unwrap();

        let p = ChannelPublishPayload {
            r#type: "message".to_string(),
            data: serde_json::json!(null),
        };
        assert_eq!(store.append("ch", &p).unwrap().id, "1");
        assert_eq!(store.append("ch", &p).unwrap().id, "2");
        assert_eq!(store.append("ch", &p).unwrap().id, "3");
    }
}
