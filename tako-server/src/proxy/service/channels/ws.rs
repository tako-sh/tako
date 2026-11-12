use crate::channels::ChannelAuthResponse;
use crate::native_channel_client::ProxyChannelStore;
use crate::proxy::TakoProxy;
use pingora_core::prelude::*;
use pingora_proxy::Session;
use std::sync::Arc;
use tako_channels::authorize::{AppAuthorizer, authorize_first_frame};
use tako_channels::websocket::{WebSocketFrameReader, build_websocket_upgrade_response};

pub(super) enum ChannelWebSocketAuth {
    Authorized(ChannelAuthResponse),
    FirstFrame {
        app: AppAuthorizer,
        params: serde_json::Value,
        cookie: Option<String>,
    },
}

impl TakoProxy {
    pub(super) async fn write_channel_websocket(
        &self,
        session: &mut Session,
        store: &Arc<ProxyChannelStore>,
        stream: &tako_channels::ChannelStream,
        requested: tako_channels::ReplayFrom,
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
        let (auth, requested) = match auth_mode {
            ChannelWebSocketAuth::Authorized(auth) => (auth, requested),
            ChannelWebSocketAuth::FirstFrame {
                app,
                params,
                cookie,
            } => match authorize_first_frame(
                session,
                &mut reader,
                &app,
                stream.channel(),
                params,
                cookie,
                requested,
            )
            .await?
            {
                Some(authorized) => authorized,
                None => return Ok(true),
            },
        };

        tako_channels::websocket::serve_with_writer(
            session,
            store.store(),
            stream,
            requested,
            auth,
            presence_params,
            reader,
            store.clone(),
        )
        .await
    }
}
