use super::*;
use crate::{
    ChannelAuthResponse, ChannelStore, ChannelStream, PRESENCE_CHANNEL_PREFIX, PresenceSession,
};
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_proxy::Session;
use std::{sync::Arc, time::Duration};

async fn write_close(
    session: &mut dyn io::ChannelIo,
    code: crate::ChannelCloseCode,
    end: bool,
) -> Result<bool> {
    session
        .write_response_body(
            Some(Bytes::from(websocket_close_frame(
                code.ws_close_code(),
                code.name(),
            ))),
            end,
        )
        .await?;
    Ok(true)
}

/// Serve an upgraded, authorized channel socket in production or local dev.
/// `params` are the validated connection params `stream` was bound with.
pub async fn serve(
    session: &mut Session,
    store: &Arc<ChannelStore>,
    stream: &ChannelStream,
    requested: crate::ReplayFrom,
    auth: ChannelAuthResponse,
    params: serde_json::Value,
    reader: WebSocketFrameReader,
) -> Result<bool> {
    serve_with_writer(
        session,
        store,
        stream,
        requested,
        auth,
        params,
        reader,
        Arc::new(crate::LocalChannelWriter::new(store.clone())),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_with_writer(
    session: &mut Session,
    store: &Arc<ChannelStore>,
    stream: &ChannelStream,
    requested: crate::ReplayFrom,
    auth: ChannelAuthResponse,
    params: serde_json::Value,
    reader: WebSocketFrameReader,
    writer: Arc<dyn crate::ChannelWriter>,
) -> Result<bool> {
    serve_on(
        &mut io::SessionIo(session),
        store,
        stream,
        requested,
        auth,
        params,
        reader,
        writer,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_on(
    session: &mut dyn io::ChannelIo,
    store: &Arc<ChannelStore>,
    stream: &ChannelStream,
    requested: crate::ReplayFrom,
    auth: ChannelAuthResponse,
    params: serde_json::Value,
    mut reader: WebSocketFrameReader,
    writer: Arc<dyn crate::ChannelWriter>,
) -> Result<bool> {
    // Storage, cursors and fanout use the stream key; frames carry the public name.
    let (channel, key) = (stream.channel(), stream.key());
    let page_size = session.replay_page_size();
    if channel.starts_with(crate::live_store::STORE_CHANNEL_PREFIX) {
        return write_close(session, crate::ChannelCloseCode::VerifyRejected, true).await;
    }
    let mut resets = store.resets(key);
    let mut changes = store.changes(key);
    if !auth.ephemeral
        && let Err(error) = writer.sync_channel(key, &auth).await
    {
        tracing::error!("failed to sync websocket channel metadata: {error}");
        return write_close(session, crate::ChannelCloseCode::VerifyRejected, true).await;
    }

    let mut ephemeral = if auth.ephemeral {
        match store.subscribe_ephemeral(key) {
            Ok(events) => Some(events),
            Err(error) => {
                tracing::warn!(%error, "ephemeral channel unavailable");
                return write_close(session, crate::ChannelCloseCode::VerifyRejected, true).await;
            }
        }
    } else {
        None
    };
    let mut presence = if channel.starts_with(PRESENCE_CHANNEL_PREFIX) {
        let scope = crate::presence_scope(channel, &params);
        let presence =
            PresenceSession::new_with_writer(writer.clone(), &scope, auth.subject.clone())
                .map_err(|e| Error::explain(ErrorType::InternalError, e.to_string()))?;
        Some(presence)
    } else {
        None
    };
    // Nothing is retained on an ephemeral stream, so it always starts live.
    let mut after = if auth.ephemeral {
        None
    } else {
        match store.replay_from_async(key, requested).await {
            Ok(after) => after,
            Err(ChannelError::StaleCursor) => {
                return write_close(session, crate::ChannelCloseCode::ReplayTooOld, true).await;
            }
            Err(error) => {
                tracing::error!("failed to resolve websocket replay cursor: {error}");
                return write_close(session, crate::ChannelCloseCode::VerifyRejected, true).await;
            }
        }
    };

    let mut read_replay = presence.is_none() && !auth.ephemeral;
    if read_replay {
        match crate::sse::closed_before(store, stream, after).await {
            Ok(Some(_)) => {
                return write_close(session, crate::ChannelCloseCode::StreamClosed, true).await;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!("failed to read websocket channel state: {error}");
                return write_close(session, crate::ChannelCloseCode::VerifyRejected, true).await;
            }
        }
    }
    session.ready().await?;
    if presence.is_some() {
        session
            .write_response_body(
                Some(Bytes::from(websocket_text_frame(
                    r#"{"type":"tako.presence.ready"}"#,
                ))),
                false,
            )
            .await?;
    }
    let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
    let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));
    let started_at = tokio::time::Instant::now();
    let mut next_ping = started_at + keepalive_interval;
    let mut buffered = true;

    loop {
        if resets.has_changed().unwrap_or(true) {
            return write_close(session, crate::ChannelCloseCode::ReplayTooOld, true).await;
        }
        let next_presence = tokio::time::Instant::now() + Duration::from_secs(1);
        if let Some(presence) = &mut presence {
            changes.borrow_and_update();
            if let Some(snapshot) = presence
                .snapshot()
                .await
                .map_err(|e| Error::explain(ErrorType::InternalError, e.to_string()))?
            {
                session
                    .write_response_body(Some(Bytes::from(websocket_text_frame(&snapshot))), false)
                    .await?;
            }
        }
        let messages = if read_replay {
            changes.borrow_and_update();
            match store.read_after_async(key, after, page_size as u32).await {
                Ok(messages) => messages,
                Err(ChannelError::StaleCursor) => {
                    return write_close(session, crate::ChannelCloseCode::ReplayTooOld, true).await;
                }
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
        if resets.has_changed().unwrap_or(true) {
            return write_close(session, crate::ChannelCloseCode::ReplayTooOld, true).await;
        }
        read_replay = messages.len() == page_size;

        if !messages.is_empty() {
            for message in messages {
                // Every earlier message was sent; the close frame is final.
                if crate::is_close_marker(&message) {
                    return write_close(session, crate::ChannelCloseCode::StreamClosed, true).await;
                }
                let message = stream.public(message);
                let encoded = match io::encode(&message, session.message_limit()) {
                    Ok(encoded) => encoded,
                    Err(_) => {
                        session
                            .write_response_body(
                                Some(Bytes::from(websocket_close_frame(
                                    1009,
                                    "message too large",
                                ))),
                                true,
                            )
                            .await?;
                        return Ok(true);
                    }
                };
                session
                    .write_response_body(Some(Bytes::from(websocket_text_frame(&encoded))), false)
                    .await?;
                after = Some(
                    message
                        .id
                        .parse::<crate::ReplayCursor>()
                        .expect("channel IDs are replay cursors"),
                );
            }
            next_ping = tokio::time::Instant::now() + keepalive_interval;
        }

        if started_at.elapsed() >= max_connection_lifetime {
            session
                .write_response_body(
                    Some(Bytes::from(websocket_close_frame(
                        1000,
                        "connection expired",
                    ))),
                    true,
                )
                .await?;
            return Ok(true);
        }

        let mut sleep_until = next_ping.min(started_at + max_connection_lifetime);
        if presence.is_some() {
            sleep_until = sleep_until.min(next_presence);
        }
        enum WebSocketAction {
            Read(Option<Bytes>),
            Tick,
            Changed,
            Reset,
            Ephemeral(Option<Arc<crate::ChannelMessage>>),
        }

        let action = if buffered {
            buffered = false;
            WebSocketAction::Read(Some(Bytes::new()))
        } else {
            let mut sleep = std::pin::pin!(tokio::time::sleep_until(sleep_until));
            let mut read = std::pin::pin!(session.read());
            tokio::select! {
                body = &mut read => WebSocketAction::Read(body?),
                _ = &mut sleep => WebSocketAction::Tick,
                _ = changes.changed(), if !auth.ephemeral => WebSocketAction::Changed,
                _ = std::future::ready(()), if read_replay => WebSocketAction::Changed,
                _ = resets.changed() => WebSocketAction::Reset,
                message = async {
                    match ephemeral.as_mut() {
                        Some(events) => events.recv().await,
                        None => std::future::pending().await,
                    }
                } => WebSocketAction::Ephemeral(message),
            }
        };

        match action {
            WebSocketAction::Read(Some(chunk)) => {
                reader.extend(&chunk);
                while let Some(frame) = reader.next_frame().map_err(|error| {
                    Error::explain(
                        ErrorType::InvalidHTTPHeader,
                        format!("Invalid websocket frame: {error}"),
                    )
                })? {
                    match frame.opcode {
                        0x1 => {
                            if is_websocket_auth_payload(&frame.payload) {
                                continue;
                            }
                            if let Some(presence) = &mut presence {
                                if presence.update(&frame.payload).await.is_err() {
                                    session
                                        .write_response_body(
                                            Some(Bytes::from(websocket_close_frame(
                                                4400,
                                                "invalid presence update",
                                            ))),
                                            true,
                                        )
                                        .await?;
                                    return Ok(true);
                                }
                                continue;
                            }
                            let payload =
                                parse_publish_payload(&frame.payload).map_err(|error| {
                                    Error::explain(
                                        ErrorType::InvalidHTTPHeader,
                                        format!("Invalid websocket publish payload: {error}"),
                                    )
                                })?;
                            let result = writer.publish(key, &payload, auth.ephemeral).await;
                            if matches!(result, Err(ChannelError::StreamClosed)) {
                                return write_close(
                                    session,
                                    crate::ChannelCloseCode::StreamClosed,
                                    true,
                                )
                                .await;
                            }
                            result.map_err(|error| {
                                Error::explain(
                                    ErrorType::InternalError,
                                    format!("Failed to append websocket channel payload: {error}"),
                                )
                            })?;
                        }
                        0x8 => {
                            session
                                .write_response_body(
                                    Some(Bytes::from(websocket_close_frame(1000, "closing"))),
                                    true,
                                )
                                .await?;
                            return Ok(true);
                        }
                        0x9 => {
                            session
                                .write_response_body(
                                    Some(Bytes::from(websocket_pong_frame(&frame.payload))),
                                    false,
                                )
                                .await?;
                        }
                        0xA => {}
                        _ => {
                            session
                                .write_response_body(
                                    Some(Bytes::from(websocket_close_frame(
                                        1003,
                                        "unsupported frame",
                                    ))),
                                    true,
                                )
                                .await?;
                            return Ok(true);
                        }
                    }
                }
                next_ping = tokio::time::Instant::now() + keepalive_interval;
            }
            WebSocketAction::Ephemeral(Some(message)) => {
                let message = stream.public((*message).clone());
                let encoded = match io::encode(&message, session.message_limit()) {
                    Ok(encoded) => encoded,
                    Err(_) => {
                        session
                            .write_response_body(
                                Some(Bytes::from(websocket_close_frame(
                                    1009,
                                    "message too large",
                                ))),
                                true,
                            )
                            .await?;
                        return Ok(true);
                    }
                };
                session
                    .write_response_body(Some(Bytes::from(websocket_text_frame(&encoded))), false)
                    .await?;
                next_ping = tokio::time::Instant::now() + keepalive_interval;
            }
            WebSocketAction::Ephemeral(None) => {
                return write_close(session, crate::ChannelCloseCode::ReplayTooOld, true).await;
            }
            WebSocketAction::Read(None) => return Ok(true),
            WebSocketAction::Reset => {
                return write_close(session, crate::ChannelCloseCode::ReplayTooOld, true).await;
            }
            WebSocketAction::Changed => read_replay = presence.is_none(),
            WebSocketAction::Tick => {
                if tokio::time::Instant::now() >= next_ping {
                    session
                        .write_response_body(Some(Bytes::from(websocket_ping_frame(b""))), false)
                        .await?;
                    next_ping = tokio::time::Instant::now() + keepalive_interval;
                }
            }
        }
    }
}
