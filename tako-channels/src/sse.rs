//! Durable SSE delivery shared by the production proxy and local dev.

use crate::{ChannelAuthResponse, ChannelError, ChannelStore, ChannelStream, ReplayCursor};
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_proxy::Session;
use std::{sync::Arc, time::Duration};

/// Control event telling the reader to drop its cursor and reconnect.
const RESET_FRAME: &[u8] = b"id:\nevent: tako.reset\ndata: {}\n\n";

/// Replay retained messages after `after`, then follow the stream until the
/// connection lifetime ends. The caller subscribes to `resets` before resolving
/// `after`, and writes the response headers before calling this.
pub async fn serve_events(
    session: &mut Session,
    store: &Arc<ChannelStore>,
    stream: &ChannelStream,
    mut after: Option<ReplayCursor>,
    auth: &ChannelAuthResponse,
    mut resets: tokio::sync::watch::Receiver<u64>,
) -> Result<bool> {
    let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
    let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));
    let started_at = tokio::time::Instant::now();
    let mut next_keepalive = started_at + keepalive_interval;
    let mut changes = store.changes(stream.key());
    let mut read_replay = true;

    loop {
        if resets.has_changed().unwrap_or(true) {
            return write_reset(session).await;
        }
        let messages = if read_replay {
            changes.borrow_and_update();
            match store.read_after_async(stream.key(), after, 100).await {
                Ok(messages) => messages,
                Err(ChannelError::StaleCursor) => return write_reset(session).await,
                Err(error) => {
                    return Err(Error::explain(
                        ErrorType::InternalError,
                        format!("Failed to read channel replay: {error}"),
                    ));
                }
            }
        } else {
            Vec::new()
        };
        read_replay = messages.len() == 100;

        if !messages.is_empty() {
            for message in messages {
                if resets.has_changed().unwrap_or(true) {
                    return write_reset(session).await;
                }
                // Storage names the stream by its key; clients see the channel.
                let message = stream.public(message);
                let encoded = serde_json::to_string(&message).map_err(|error| {
                    Error::explain(
                        ErrorType::InternalError,
                        format!("Failed to encode SSE payload: {error}"),
                    )
                })?;
                let frame = format!("id: {}\ndata: {}\n\n", message.id, encoded);
                session
                    .write_response_body(Some(Bytes::from(frame)), false)
                    .await?;
                after = Some(
                    message
                        .id
                        .parse::<ReplayCursor>()
                        .expect("stored channel cursor is valid"),
                );
            }
            next_keepalive = tokio::time::Instant::now() + keepalive_interval;
        }

        if started_at.elapsed() >= max_connection_lifetime {
            session.write_response_body(None, true).await?;
            return Ok(true);
        }

        let now = tokio::time::Instant::now();
        if now >= next_keepalive {
            session
                .write_response_body(Some(Bytes::from_static(b": keepalive\n\n")), false)
                .await?;
            next_keepalive = now + keepalive_interval;
        }

        if read_replay {
            continue;
        }
        tokio::select! {
            _ = resets.changed() => return write_reset(session).await,
            _ = changes.changed() => read_replay = true,
            _ = tokio::time::sleep_until(next_keepalive.min(started_at + max_connection_lifetime)) => {}
        }
    }
}

async fn write_reset(session: &mut Session) -> Result<bool> {
    session
        .write_response_body(Some(Bytes::from_static(RESET_FRAME)), true)
        .await?;
    Ok(true)
}
