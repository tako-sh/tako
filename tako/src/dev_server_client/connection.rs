use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub(super) const DEV_SERVER_CONNECTION_CLOSED_MESSAGE: &str = "dev-server closed connection";

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

pub(super) async fn ping(c: &mut LineClient) -> Result<(), Box<dyn std::error::Error>> {
    c.send_line(r#"{"type":"Ping"}"#).await?;
    let line = c.read_line().await?;
    if line.trim() == r#"{"type":"Pong"}"# {
        return Ok(());
    }
    Err(format!("unexpected response: {}", line).into())
}
