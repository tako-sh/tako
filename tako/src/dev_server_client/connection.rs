use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub(super) const DEV_SERVER_CONNECTION_CLOSED_MESSAGE: &str = "dev-server closed connection";
pub(crate) const DAEMON_UPGRADE_HINT: &str = "Development daemon protocol does not match this CLI. Run `tako dev --restart` with the updated CLI and daemon; this stops other local apps on that daemon.";

pub(super) fn socket_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(crate::paths::tako_data_dir()?.join("dev-server.sock"))
}

pub(crate) struct LineClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl LineClient {
    pub(super) fn new(stream: UnixStream) -> Self {
        let (r, w) = stream.into_split();
        Self {
            reader: BufReader::new(r),
            writer: w,
        }
    }

    pub(super) async fn send_line(&mut self, s: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.writer.write_all(s.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }

    pub(super) async fn read_line(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                DEV_SERVER_CONNECTION_CLOSED_MESSAGE,
            )
            .into());
        }
        Ok(line)
    }
}

pub(crate) fn current_protocol(response: &serde_json::Value) -> bool {
    response.get("type").and_then(|v| v.as_str()) == Some("Info")
        && response
            .pointer("/info/protocol_version")
            .and_then(|v| v.as_u64())
            == Some(u64::from(tako_core::PROTOCOL_VERSION))
}

/// Require the current daemon contract before reading environment-scoped registrations.
pub(super) async fn check_protocol(c: &mut LineClient) -> Result<(), Box<dyn std::error::Error>> {
    c.send_line(r#"{"type":"Info"}"#).await?;
    let line = c.read_line().await?;
    let response: serde_json::Value = serde_json::from_str(&line)?;
    if current_protocol(&response) {
        return Ok(());
    }
    Err(DAEMON_UPGRADE_HINT.into())
}
