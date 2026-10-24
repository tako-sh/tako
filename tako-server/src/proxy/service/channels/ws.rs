use crate::channels::{ChannelAuthResponse, ChannelError, ChannelOperation, ChannelStore};
use crate::proxy::TakoProxy;
use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_proxy::Session;
use std::sync::Arc;
use std::time::Duration;
use tako_channels::close_codes::ChannelCloseCode;
use tako_channels::websocket::{
    WebSocketFrameReader, build_websocket_upgrade_response, parse_first_frame,
    websocket_close_frame, websocket_pong_frame,
};

pub(super) enum ChannelWebSocketAuth {
    Authorized(ChannelAuthResponse),
    FirstFrame {
        endpoint: String,
        internal_host: String,
        internal_token: String,
        params: serde_json::Value,
        cookie: Option<String>,
    },
}

impl TakoProxy {
    async fn write_websocket_close(
        &self,
        session: &mut Session,
        code: ChannelCloseCode,
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

    pub(super) async fn write_channel_websocket(
        &self,
        session: &mut Session,
        store: &Arc<ChannelStore>,
        channel: &str,
        query_after: Option<i64>,
        presence_params: serde_json::Value,
        auth_mode: ChannelWebSocketAuth,
    ) -> Result<bool> {
        if !session.as_downstream().is_upgrade_req() {
            return self
                .write_json_response(
                    session,
                    400,
                    &serde_json::json!({ "error": "WebSocket upgrade required" }),
                )
                .await;
        }

        let header = match build_websocket_upgrade_response(session.req_header()) {
            Ok(header) => header,
            Err(error) => return self.write_channel_error(session, error).await,
        };
        session
            .write_response_header(Box::new(header), false)
            .await?;

        let mut reader = WebSocketFrameReader::default();
        let (auth, requested_after) = match auth_mode {
            ChannelWebSocketAuth::Authorized(auth) => (auth, query_after),
            ChannelWebSocketAuth::FirstFrame {
                endpoint,
                internal_host,
                internal_token,
                params,
                cookie,
            } => {
                let frame = match read_first_websocket_text_frame(
                    session,
                    &mut reader,
                    Duration::from_secs(5),
                )
                .await?
                {
                    Some(frame) => frame,
                    None => {
                        return self
                            .write_websocket_close(
                                session,
                                ChannelCloseCode::AuthFrameMissing,
                                true,
                            )
                            .await;
                    }
                };

                let text = match String::from_utf8(frame) {
                    Ok(text) => text,
                    Err(_) => {
                        return self
                            .write_websocket_close(
                                session,
                                ChannelCloseCode::AuthFrameMalformed,
                                true,
                            )
                            .await;
                    }
                };

                let parsed = match parse_first_frame(&text) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        return self
                            .write_websocket_close(
                                session,
                                ChannelCloseCode::AuthFrameMalformed,
                                true,
                            )
                            .await;
                    }
                };

                let auth = match tako_channels::authorize_channel_request(
                    &endpoint,
                    &internal_host,
                    crate::instances::INTERNAL_TOKEN_HEADER,
                    &internal_token,
                    ChannelOperation::Connect,
                    channel,
                    params,
                    parsed.header_value,
                    cookie,
                )
                .await
                {
                    Ok(auth) => auth,
                    Err(ChannelError::NotDefined) => {
                        return self
                            .write_websocket_close(session, ChannelCloseCode::ChannelUnknown, true)
                            .await;
                    }
                    Err(_) => {
                        return self
                            .write_websocket_close(session, ChannelCloseCode::VerifyRejected, true)
                            .await;
                    }
                };

                (auth, parsed.last_message_id.or(query_after))
            }
        };

        tako_channels::websocket::serve(
            session,
            store,
            channel,
            requested_after,
            auth,
            presence_params,
            reader,
        )
        .await
    }
}

async fn read_first_websocket_text_frame(
    session: &mut Session,
    reader: &mut WebSocketFrameReader,
    timeout: Duration,
) -> Result<Option<Vec<u8>>> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        enum FirstFrameAction {
            Read(Option<Bytes>),
            Timeout,
        }

        let action = {
            let mut sleep = std::pin::pin!(tokio::time::sleep_until(deadline));
            let mut read = std::pin::pin!(session.as_downstream_mut().read_body_or_idle(false));
            tokio::select! {
                body = &mut read => FirstFrameAction::Read(body?),
                _ = &mut sleep => FirstFrameAction::Timeout,
            }
        };

        match action {
            FirstFrameAction::Timeout | FirstFrameAction::Read(None) => return Ok(None),
            FirstFrameAction::Read(Some(chunk)) => {
                reader.extend(&chunk);
                while let Some(frame) = reader.next_frame().map_err(|error| {
                    Error::explain(
                        ErrorType::InvalidHTTPHeader,
                        format!("Invalid websocket frame: {error}"),
                    )
                })? {
                    match frame.opcode {
                        0x1 => return Ok(Some(frame.payload)),
                        0x8 => return Ok(None),
                        0x9 => {
                            session
                                .write_response_body(
                                    Some(Bytes::from(websocket_pong_frame(&frame.payload))),
                                    false,
                                )
                                .await?;
                        }
                        0xA => {}
                        _ => return Ok(None),
                    }
                }
            }
        }
    }
}
