use super::*;

impl TakoProxy {
    pub(super) async fn write_channel_events(
        &self,
        session: &mut Session,
        store: &Arc<ChannelStore>,
        channel: &str,
        mut after: Option<ReplayCursor>,
        auth: &ChannelAuthResponse,
        mut resets: tokio::sync::watch::Receiver<u64>,
    ) -> Result<bool> {
        let mut header = ResponseHeader::build(200, None)?;
        header.insert_header("Content-Type", "text/event-stream")?;
        header.insert_header("Cache-Control", "no-store")?;
        header.insert_header("Connection", "keep-alive")?;
        session
            .write_response_header(Box::new(header), false)
            .await?;

        let keepalive_interval = Duration::from_millis(auth.keepalive_interval_ms.max(1));
        let max_connection_lifetime = Duration::from_millis(auth.max_connection_lifetime_ms.max(1));
        let started_at = tokio::time::Instant::now();
        let mut next_keepalive = started_at + keepalive_interval;
        let mut changes = store.changes(channel);
        let mut read_replay = true;

        loop {
            if resets.has_changed().unwrap_or(true) {
                session
                    .write_response_body(
                        Some(Bytes::from_static(b"id:\nevent: tako.reset\ndata: {}\n\n")),
                        true,
                    )
                    .await?;
                return Ok(true);
            }
            let messages = if read_replay {
                changes.borrow_and_update();
                match store.read_after_async(channel, after, 100).await {
                    Ok(messages) => messages,
                    Err(ChannelError::StaleCursor) => {
                        session
                            .write_response_body(
                                Some(Bytes::from_static(b"id:\nevent: tako.reset\ndata: {}\n\n")),
                                true,
                            )
                            .await?;
                        return Ok(true);
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
            read_replay = messages.len() == 100;

            if !messages.is_empty() {
                for message in messages {
                    if resets.has_changed().unwrap_or(true) {
                        session
                            .write_response_body(
                                Some(Bytes::from_static(b"id:\nevent: tako.reset\ndata: {}\n\n")),
                                true,
                            )
                            .await?;
                        return Ok(true);
                    }
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
                _ = resets.changed() => {
                    session.write_response_body(Some(Bytes::from_static(b"id:\nevent: tako.reset\ndata: {}\n\n")), true).await?;
                    return Ok(true);
                },
                _ = changes.changed() => read_replay = true,
                _ = tokio::time::sleep_until(next_keepalive.min(started_at + max_connection_lifetime)) => {}
            }
        }
    }
}
