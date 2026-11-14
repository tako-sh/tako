//! Multiplexed browser transport. Authorization, replay and presence belong to
//! subscriptions; transport liveness and bounded writes belong to the socket.

mod subscription;
#[cfg(test)]
mod tests;

use crate::websocket::{
    WebSocketFrameReader, build_websocket_upgrade_response, websocket_close_frame,
    websocket_ping_frame, websocket_pong_frame, websocket_text_frame,
};
use crate::{ChannelStore, ChannelWriter, authorize::AppAuthorizer};
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_proxy::Session;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc};

pub const PATH: &str = "/_tako/channels/ws";
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_SUBSCRIPTIONS: usize = 32;
const CHUNK_BYTES: usize = 8 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(75);

/// Resolve authorization against the current deployment for each subscription.
#[async_trait::async_trait]
pub trait AppResolver: Send + Sync {
    async fn resolve(&self) -> std::result::Result<AppAuthorizer, crate::ChannelError>;
}

#[async_trait::async_trait]
impl AppResolver for AppAuthorizer {
    async fn resolve(&self) -> std::result::Result<AppAuthorizer, crate::ChannelError> {
        Ok(self.clone())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Command {
    Subscribe {
        id: String,
        channel: String,
        #[serde(default)]
        query: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },
    Unsubscribe {
        id: String,
    },
    Send {
        id: String,
        data: Value,
    },
}

struct Subscription {
    generation: u64,
    input: mpsc::Sender<Bytes>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Output {
    id: String,
    generation: u64,
    body: Value,
    terminal: bool,
}

async fn write(session: &mut Session, bytes: Vec<u8>, end: bool) -> Result<()> {
    tokio::time::timeout(
        WRITE_TIMEOUT,
        session.write_response_body(Some(Bytes::from(bytes)), end),
    )
    .await
    .map_err(|_| Error::explain(ErrorType::WriteTimedout, "slow channel reader"))?
}

async fn control(session: &mut Session, value: Value) -> Result<()> {
    write(session, websocket_text_frame(&value.to_string()), false).await
}

/// Serve a shared connection for one app/environment. The caller resolves the
/// app and its writer, but each subscription verifies its own params and auth.
pub async fn serve(
    session: &mut Session,
    app: Arc<dyn AppResolver>,
    store: Arc<ChannelStore>,
    writer: Arc<dyn ChannelWriter>,
    is_https: bool,
) -> Result<bool> {
    if session.req_header().method.as_str() != "GET" || !session.as_downstream().is_upgrade_req() {
        let header = pingora_http::ResponseHeader::build(426, None)?;
        session
            .write_response_header(Box::new(header), true)
            .await?;
        return Ok(true);
    }
    let request = session.req_header().clone();
    let header = build_websocket_upgrade_response(&request)
        .map_err(|e| Error::explain(ErrorType::InvalidHTTPHeader, e.to_string()))?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    let mut reader = WebSocketFrameReader::bounded(MAX_FRAME_BYTES as u64, true);
    // At most 16 bounded frames are queued, independent of producer speed.
    let (output_tx, mut output_rx) = mpsc::channel::<Output>(16);
    // Terminal delivery must not depend on capacity in the data queue. If a
    // subscription cannot enqueue its close, disconnect so clients can recover.
    let delivery_failed = Arc::new(Notify::new());
    let mut subscriptions = HashMap::<String, Subscription>::new();
    let mut generation = 0u64;
    let mut last_activity = tokio::time::Instant::now();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;
    loop {
        enum Action {
            Input(Option<Bytes>),
            Output(Output),
            Heartbeat,
            DeliveryFailed,
        }
        let action = tokio::select! {
            _ = delivery_failed.notified() => Action::DeliveryFailed,
            body = session.as_downstream_mut().read_body_or_idle(false) => Action::Input(body?),
            Some(output) = output_rx.recv() => Action::Output(output),
            _ = heartbeat.tick() => Action::Heartbeat,
        };
        match action {
            Action::DeliveryFailed => {
                write(
                    session,
                    websocket_close_frame(1012, "slow channel reader; reconnect"),
                    true,
                )
                .await?;
                return Ok(true);
            }
            Action::Heartbeat => {
                if last_activity.elapsed() >= HEARTBEAT_TIMEOUT {
                    write(
                        session,
                        websocket_close_frame(1001, "heartbeat expired"),
                        true,
                    )
                    .await?;
                    return Ok(true);
                }
                write(session, websocket_ping_frame(b"tako"), false).await?;
            }
            Action::Output(output) => {
                if !subscriptions
                    .get(&output.id)
                    .is_some_and(|s| s.generation == output.generation)
                {
                    continue;
                }
                // An app may have been redeployed or scaled to zero since the
                // handshake resolved its authorize endpoint. Reconnect through
                // the proxy so new subscriptions don't stay pinned to it.
                let resolve_app_again = output.terminal && output.body["code"] == 1013;
                control(session, output.body).await?;
                if output.terminal {
                    subscriptions.remove(&output.id);
                }
                if resolve_app_again {
                    write(
                        session,
                        websocket_close_frame(1012, "authorization unavailable; reconnect"),
                        true,
                    )
                    .await?;
                    return Ok(true);
                }
            }
            Action::Input(None) => return Ok(true),
            Action::Input(Some(bytes)) => {
                reader.extend(&bytes);
                loop {
                    let frame = match reader.next_frame() {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(error) => {
                            let code = match &error {
                                crate::ChannelError::BadRequest(reason)
                                    if reason == "websocket frame too large" =>
                                {
                                    1009
                                }
                                _ => 1002,
                            };
                            write(
                                session,
                                websocket_close_frame(code, "invalid or oversized frame"),
                                true,
                            )
                            .await?;
                            return Ok(true);
                        }
                    };
                    last_activity = tokio::time::Instant::now();
                    match frame.opcode {
                        0x8 => {
                            write(session, websocket_close_frame(1000, "closing"), true).await?;
                            return Ok(true);
                        }
                        0x9 => {
                            write(session, websocket_pong_frame(&frame.payload), false).await?;
                            continue;
                        }
                        0xa => continue,
                        0x1 => {}
                        _ => {
                            write(session, websocket_close_frame(1003, "text required"), true)
                                .await?;
                            return Ok(true);
                        }
                    }
                    let command = match serde_json::from_slice::<Command>(&frame.payload) {
                        Ok(command) => command,
                        Err(_) => {
                            write(
                                session,
                                websocket_close_frame(1002, "invalid channel command"),
                                true,
                            )
                            .await?;
                            return Ok(true);
                        }
                    };
                    let id = match &command {
                        Command::Subscribe { id, .. }
                        | Command::Unsubscribe { id }
                        | Command::Send { id, .. } => id,
                    };
                    if id.is_empty() || id.len() > 64 {
                        write(
                            session,
                            websocket_close_frame(1002, "invalid subscription id"),
                            true,
                        )
                        .await?;
                        return Ok(true);
                    }
                    match command {
                        Command::Subscribe {
                            id,
                            channel,
                            query,
                            headers,
                            token,
                            cursor,
                            from,
                        } => {
                            if subscriptions.contains_key(&id) {
                                write(
                                    session,
                                    websocket_close_frame(1002, "invalid subscription id"),
                                    true,
                                )
                                .await?;
                                return Ok(true);
                            }
                            if subscriptions.len() >= MAX_SUBSCRIPTIONS {
                                control(session, json!({"type":"closed", "id":id, "code":4429, "reason":"subscription limit"})).await?;
                                continue;
                            }
                            generation += 1;
                            let (input, rx) = mpsc::channel(2);
                            let io = subscription::SubscriptionIo::new(
                                id.clone(),
                                generation,
                                rx,
                                output_tx.clone(),
                                delivery_failed.clone(),
                            );
                            let spec = subscription::Spec {
                                channel,
                                query,
                                headers,
                                token,
                                cursor,
                                from,
                            };
                            let app = app.clone();
                            let request = request.clone();
                            let store = store.clone();
                            let writer = writer.clone();
                            let task = tokio::spawn(async move {
                                subscription::run(io, spec, app, request, store, writer, is_https)
                                    .await;
                            });
                            subscriptions.insert(
                                id,
                                Subscription {
                                    generation,
                                    input,
                                    task,
                                },
                            );
                        }
                        Command::Unsubscribe { id } => {
                            // Dropping a task cancels auth/replay and its presence session too.
                            subscriptions.remove(&id);
                            control(session, json!({"type":"unsubscribed", "id":id})).await?;
                        }
                        Command::Send { id, data } => {
                            let Some(subscription) = subscriptions.get(&id) else {
                                continue;
                            };
                            if subscription
                                .input
                                .try_send(Bytes::from(websocket_text_frame(&data.to_string())))
                                .is_err()
                            {
                                subscriptions.remove(&id);
                                control(session, json!({"type":"closed", "id":id, "code":4429, "reason":"publish queue full"})).await?;
                            }
                        }
                    }
                }
            }
        }
    }
}
