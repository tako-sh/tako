//! Channel delivery independent of the physical connection. A shared socket
//! gives each subscription its own cancellable I/O endpoint.

use bytes::Bytes;
use pingora_core::prelude::*;
use pingora_proxy::Session;

#[async_trait::async_trait]
pub(crate) trait ChannelIo: Send {
    async fn read(&mut self) -> Result<Option<Bytes>>;
    async fn write_response_body(&mut self, body: Option<Bytes>, end: bool) -> Result<()>;

    /// Acknowledge only after replay position and live watchers are installed.
    async fn ready(&mut self) -> Result<()> {
        Ok(())
    }

    fn replay_page_size(&self) -> usize {
        100
    }

    fn message_limit(&self) -> usize {
        usize::MAX
    }
}

/// Stop serialization as soon as a logical message exceeds its budget.
pub(crate) fn encode(
    value: &impl serde::Serialize,
    limit: usize,
) -> std::result::Result<String, serde_json::Error> {
    struct Limited {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > self.limit {
                return Err(std::io::Error::other("channel message too large"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(String::from_utf8(writer.bytes).expect("JSON is UTF-8"))
}

pub(crate) struct SessionIo<'a>(pub &'a mut Session);

#[async_trait::async_trait]
impl ChannelIo for SessionIo<'_> {
    async fn read(&mut self) -> Result<Option<Bytes>> {
        self.0.as_downstream_mut().read_body_or_idle(false).await
    }

    async fn write_response_body(&mut self, body: Option<Bytes>, end: bool) -> Result<()> {
        self.0.write_response_body(body, end).await
    }
}
