//! Durable SSE delivery shared by the production proxy and local dev.

use crate::{
    ChannelAuthResponse, ChannelError, ChannelMessage, ChannelStore, ChannelStream, ReplayCursor,
    is_close_marker,
};
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_proxy::Session;
use std::{sync::Arc, time::Duration};

/// Control event telling the reader to drop its cursor and reconnect.
const RESET_FRAME: &[u8] = b"id:\nevent: tako.reset\ndata: {}\n\n";
/// Name of the final control event of a closed stream.
pub const CLOSE_EVENT: &str = "tako.close";

/// Whether a reader positioned at `after` has already passed the stream's
/// terminal marker, so the next thing it receives is the terminal signal.
/// A reader with no cursor replays from the start and meets the marker in
/// order. The caller resolved `after` against this stream.
pub(crate) async fn closed_before(
    store: &Arc<ChannelStore>,
    stream: &ChannelStream,
    after: Option<ReplayCursor>,
) -> Result<Option<ChannelMessage>, ChannelError> {
    let Some(after) = after else {
        return Ok(None);
    };
    Ok(store
        .close_marker_async(stream.key())
        .await?
        .filter(|marker| {
            marker
                .id
                .parse::<ReplayCursor>()
                .is_ok_and(|marker| marker.position <= after.position)
        }))
}

/// Replay retained messages after `after`, then follow the stream until the
/// connection lifetime ends or the stream's terminal marker is reached. `None`
/// replays every retained message. The caller subscribes to `resets` before
/// resolving `after`, and writes the response headers before calling this.
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
    match closed_before(store, stream, after).await {
        Ok(Some(marker)) => return write_close(session, &marker).await,
        Ok(None) => {}
        Err(error) => {
            return Err(Error::explain(
                ErrorType::InternalError,
                format!("Failed to read channel state: {error}"),
            ));
        }
    }

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
                if is_close_marker(&message) {
                    return write_close(session, &message).await;
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

/// The terminal signal: every earlier message was sent, none follows. Its id
/// is the marker's cursor, so a reader resuming from it is told again.
fn close_frame(marker: &ChannelMessage) -> String {
    format!("id: {}\nevent: {CLOSE_EVENT}\ndata: {{}}\n\n", marker.id)
}

async fn write_close(session: &mut Session, marker: &ChannelMessage) -> Result<bool> {
    session
        .write_response_body(Some(Bytes::from(close_frame(marker))), true)
        .await?;
    Ok(true)
}

async fn write_reset(session: &mut Session) -> Result<bool> {
    session
        .write_response_body(Some(Bytes::from_static(RESET_FRAME)), true)
        .await?;
    Ok(true)
}
