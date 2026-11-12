use super::*;

impl TakoProxy {
    pub(super) async fn write_channel_events(
        &self,
        session: &mut Session,
        store: &Arc<ChannelStore>,
        stream: &ChannelStream,
        after: Option<ReplayCursor>,
        auth: &ChannelAuthResponse,
        resets: tokio::sync::watch::Receiver<u64>,
    ) -> Result<bool> {
        let mut header = ResponseHeader::build(200, None)?;
        header.insert_header("Content-Type", "text/event-stream")?;
        header.insert_header("Cache-Control", "no-store")?;
        header.insert_header("Connection", "keep-alive")?;
        session
            .write_response_header(Box::new(header), false)
            .await?;
        tako_channels::sse::serve_events(session, store, stream, after, auth, resets).await
    }
}
