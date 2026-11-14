use super::*;
use crate::authorize::{request_credentials, require_websocket_grant};
use crate::live_store::{STORE_CHANNEL_PREFIX, internal_store_json};
use crate::websocket::io::ChannelIo;
use crate::{
    ChannelAuthResponse, ChannelDefinitionMeta, ChannelError, ChannelOperation, ChannelStream,
    ReplayFrom,
};

pub(super) struct Spec {
    pub channel: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub token: Option<String>,
    pub cursor: Option<String>,
    pub from: Option<String>,
}

pub(super) struct SubscriptionIo {
    id: String,
    generation: u64,
    input: mpsc::Receiver<Bytes>,
    output: mpsc::Sender<Output>,
    delivery_failed: Arc<Notify>,
    closed: bool,
}

impl SubscriptionIo {
    pub fn new(
        id: String,
        generation: u64,
        input: mpsc::Receiver<Bytes>,
        output: mpsc::Sender<Output>,
        delivery_failed: Arc<Notify>,
    ) -> Self {
        Self {
            id,
            generation,
            input,
            output,
            delivery_failed,
            closed: false,
        }
    }

    async fn emit(&mut self, mut body: Value, terminal: bool) -> Result<()> {
        if self.closed {
            return Err(Error::explain(
                ErrorType::ConnectionClosed,
                "subscription closed",
            ));
        }
        body["id"] = self.id.clone().into();
        let output = Output {
            id: self.id.clone(),
            generation: self.generation,
            body,
            terminal,
        };
        tokio::time::timeout(WRITE_TIMEOUT, self.output.send(output))
            .await
            .map_err(|_| Error::explain(ErrorType::WriteTimedout, "slow subscription reader"))?
            .map_err(|_| Error::explain(ErrorType::ConnectionClosed, "shared connection closed"))?;
        self.closed = terminal;
        Ok(())
    }

    async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let result = self
            .emit(json!({"type":"closed", "code":code, "reason":reason}), true)
            .await;
        if result.is_err() {
            self.delivery_failed.notify_one();
        }
        result
    }

    async fn text(&mut self, text: &str) -> Result<()> {
        if text.len() > MAX_MESSAGE_BYTES {
            return self
                .close(1009, "message exceeds 1 MiB; stream smaller events")
                .await;
        }
        let mut rest = text;
        loop {
            let mut end = CHUNK_BYTES.min(rest.len());
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let (chunk, remaining) = rest.split_at(end);
            self.emit(
                json!({"type":"data", "data":chunk, "end":remaining.is_empty()}),
                false,
            )
            .await?;
            if remaining.is_empty() {
                return Ok(());
            }
            rest = remaining;
        }
    }
}

#[async_trait::async_trait]
impl ChannelIo for SubscriptionIo {
    async fn read(&mut self) -> Result<Option<Bytes>> {
        Ok(self.input.recv().await)
    }

    async fn ready(&mut self) -> Result<()> {
        self.emit(json!({"type":"subscribed"}), false).await
    }

    async fn write_response_body(&mut self, body: Option<Bytes>, end: bool) -> Result<()> {
        if let Some(body) = body {
            let mut reader = WebSocketFrameReader::default();
            reader.extend(&body);
            while let Some(frame) = reader
                .next_frame()
                .map_err(|e| Error::explain(ErrorType::InternalError, e.to_string()))?
            {
                match frame.opcode {
                    0x1 => {
                        let text = std::str::from_utf8(&frame.payload).map_err(|_| {
                            Error::explain(ErrorType::InternalError, "invalid channel UTF-8")
                        })?;
                        self.text(text).await?;
                    }
                    0x8 => {
                        let code = frame
                            .payload
                            .get(..2)
                            .map(|b| u16::from_be_bytes([b[0], b[1]]))
                            .unwrap_or(1000);
                        let reason =
                            std::str::from_utf8(frame.payload.get(2..).unwrap_or_default())
                                .unwrap_or("closed");
                        return self.close(code, reason).await;
                    }
                    // Connection-level ping/pong is owned by the physical socket.
                    0x9 | 0xa => {}
                    _ => return self.close(1003, "unsupported channel frame").await,
                }
            }
        }
        if end {
            self.close(1000, "subscription ended").await?;
        }
        Ok(())
    }

    fn replay_page_size(&self) -> usize {
        1
    }
    fn message_limit(&self) -> usize {
        MAX_MESSAGE_BYTES
    }
}

async fn authorize(
    spec: &Spec,
    app: &AppAuthorizer,
    mut request: pingora_http::RequestHeader,
    is_https: bool,
) -> std::result::Result<(Value, ChannelAuthResponse), ChannelError> {
    if spec.channel.is_empty()
        || spec.channel.len() > 256
        || crate::reserved_stream_name(&spec.channel)
        || spec.query.len() > 16 * 1024 * 3 + 32
    {
        return Err(ChannelError::BadRequest("invalid subscription".into()));
    }
    let definitions = internal_store_json(
        &app.endpoint,
        &app.internal_host,
        &app.internal_token,
        crate::INTERNAL_CHANNEL_REGISTRY_PATH,
        None,
    )
    .await?;
    let meta = serde_json::from_value::<Vec<ChannelDefinitionMeta>>(definitions)
        .map_err(|_| ChannelError::AuthUnavailable)?
        .into_iter()
        .find(|meta| meta.channel == spec.channel)
        .ok_or(ChannelError::NotDefined)?;
    if spec.channel.starts_with(STORE_CHANNEL_PREFIX) {
        return Err(ChannelError::Unsupported);
    }
    let params = crate::params::validate_query(&meta.params_schema, &spec.query)
        .map_err(|e| ChannelError::BadRequest(e.to_string()))?;
    // Only the declared auth header may be supplied by a browser. Cookies,
    // origin and routing authority always come from the actual HTTP handshake.
    if let crate::ChannelAuthScheme::Required {
        header_name: Some(name),
        ..
    } = &meta.auth
        && !["cookie", "origin", "host"]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
        && let Some(value) = spec
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
            .or(spec.token.as_ref())
    {
        request
            .insert_header(name.clone(), value.clone())
            .map_err(|_| ChannelError::Unauthorized)?;
    }
    let credentials = request_credentials(&request, &meta.auth, true, is_https)?;
    if credentials.first_frame {
        return Err(ChannelError::Unauthorized);
    }
    let auth = app
        .authorize(
            ChannelOperation::Connect,
            &spec.channel,
            params.clone(),
            credentials.header,
            credentials.cookie,
        )
        .await?;
    require_websocket_grant(&auth)?;
    Ok((params, auth))
}

pub(super) async fn run(
    mut io: SubscriptionIo,
    spec: Spec,
    app: Arc<dyn AppResolver>,
    request: pingora_http::RequestHeader,
    store: Arc<ChannelStore>,
    writer: Arc<dyn ChannelWriter>,
    is_https: bool,
) {
    let authorized = tokio::time::timeout(Duration::from_secs(10), async {
        let app = app.resolve().await?;
        authorize(&spec, &app, request, is_https).await
    })
    .await;
    let (params, auth) = match authorized {
        Ok(Ok(authorized)) => authorized,
        other => {
            let (code, reason) = match other {
                Ok(Err(ChannelError::Unauthorized)) => (4401, "unauthorized"),
                Ok(Err(ChannelError::Forbidden | ChannelError::Unsupported)) => (4403, "forbidden"),
                Ok(Err(ChannelError::NotDefined)) => (4404, "channel not defined"),
                Ok(Err(ChannelError::BadRequest(_) | ChannelError::ParamsInvalid(_))) => {
                    (4400, "invalid subscription params")
                }
                _ => (1013, "authorization unavailable"),
            };
            let _ = io.close(code, reason).await;
            return;
        }
    };
    let cursor = match spec.cursor.as_deref().map(str::parse).transpose() {
        Ok(cursor) if spec.from.as_deref().is_none_or(|from| from == "start") => cursor,
        _ => {
            let _ = io.close(4400, "invalid replay position").await;
            return;
        }
    };
    let result = crate::websocket::serve_on(
        &mut io,
        &store,
        &ChannelStream::new(&spec.channel, &params),
        ReplayFrom::new(cursor, spec.from.is_some()),
        auth,
        params,
        WebSocketFrameReader::default(),
        writer,
    )
    .await;
    if let Err(error) = result {
        tracing::debug!(%error, "shared channel subscription ended");
    }
    let _ = io.close(1011, "subscription interrupted").await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_timeout_notifies_connection_even_when_output_is_full() {
        let (_input_tx, input) = mpsc::channel(1);
        let (output, mut receiver) = mpsc::channel(1);
        let failed = Arc::new(Notify::new());
        let mut io = SubscriptionIo::new("page".into(), 1, input, output, failed.clone());
        io.ready().await.unwrap();

        // Keep the queue full for the entire deadline. No consumer capacity is
        // needed to wake the socket owner and force reconnection.
        let close = tokio::spawn(async move { io.close(1011, "interrupted").await });
        tokio::time::timeout(WRITE_TIMEOUT + Duration::from_secs(2), failed.notified())
            .await
            .expect("socket owner must hear about failed terminal delivery");
        assert!(close.await.unwrap().is_err());
        assert_eq!(receiver.try_recv().unwrap().body["type"], "subscribed");
    }

    #[tokio::test]
    async fn delivered_close_and_cancelled_subscription_do_not_disconnect_peers() {
        let failed = Arc::new(Notify::new());
        let (_input_tx, input) = mpsc::channel(1);
        let (output, mut receiver) = mpsc::channel(1);
        let mut io = SubscriptionIo::new("page".into(), 1, input, output, failed.clone());
        io.close(1000, "done").await.unwrap();
        assert!(receiver.try_recv().unwrap().terminal);
        drop(io);

        let (_input_tx, input) = mpsc::channel(1);
        let (output, _receiver) = mpsc::channel(1);
        let mut io = SubscriptionIo::new("cancelled".into(), 2, input, output, failed.clone());
        io.ready().await.unwrap();
        let close = tokio::spawn(async move { io.close(1011, "interrupted").await });
        tokio::task::yield_now().await;
        close.abort();
        assert!(close.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), failed.notified())
                .await
                .is_err()
        );
    }
}
