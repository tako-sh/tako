use serde::Serialize;
use serde::de::DeserializeOwned;
use std::future::Future;
use tokio::io::BufReader;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

pub const DEFAULT_MAX_LINE_BYTES: usize = 1024 * 1024;

pub async fn read_json_line_with_limit<R, T>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    // Read incrementally, checking the limit at each chunk boundary so a
    // malicious sender without a newline cannot force unbounded allocation.
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            reader.consume(pos + 1);
            break;
        }
        buf.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
        if buf.len() > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "json line exceeds max length ({} > {})",
                    buf.len(),
                    max_bytes
                ),
            ));
        }
    }
    if buf.is_empty() {
        return Ok(None);
    }
    if buf.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "json line exceeds max length ({} > {})",
                buf.len(),
                max_bytes
            ),
        ));
    }

    let s = std::str::from_utf8(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    serde_json::from_str::<T>(s)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub async fn read_json_line<R, T>(reader: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    read_json_line_with_limit(reader, DEFAULT_MAX_LINE_BYTES).await
}

pub async fn write_json_line<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut json = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    json.push(b'\n');
    writer.write_all(&json).await?;
    Ok(())
}

pub async fn serve_jsonl_connection<Req, Resp, F, Fut, InvalidResp>(
    stream: UnixStream,
    handler: F,
    invalid_response: InvalidResp,
) -> std::io::Result<()>
where
    Req: DeserializeOwned,
    Resp: Serialize,
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Resp>,
    InvalidResp: Fn(std::io::Error) -> Resp,
{
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    loop {
        let Some(req) = (match read_json_line::<_, Req>(&mut reader).await {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                let resp = invalid_response(e);
                let _ = write_json_line(&mut writer, &resp).await;
                continue;
            }
            Err(e) => return Err(e),
        }) else {
            break;
        };

        let resp = handler(req).await;
        write_json_line(&mut writer, &resp).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::BufReader;

    struct CountingWriter {
        bytes: Vec<u8>,
        writes: usize,
    }

    impl CountingWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                writes: 0,
            }
        }
    }

    impl AsyncWrite for CountingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn roundtrips_struct_over_jsonl() {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        struct Msg {
            kind: String,
            n: u64,
        }

        let (a, b) = tokio::io::duplex(1024);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (mut br, mut bw) = tokio::io::split(b);
        let mut ar = BufReader::new(&mut ar);
        let mut br = BufReader::new(&mut br);

        let a_send = Msg {
            kind: "hello".to_string(),
            n: 42,
        };
        write_json_line(&mut aw, &a_send).await.unwrap();
        let b_recv: Msg = read_json_line(&mut br).await.unwrap().unwrap();
        assert_eq!(b_recv, a_send);

        let b_send = Msg {
            kind: "world".to_string(),
            n: 7,
        };
        write_json_line(&mut bw, &b_send).await.unwrap();
        let a_recv: Msg = read_json_line(&mut ar).await.unwrap().unwrap();
        assert_eq!(a_recv, b_send);
    }

    #[tokio::test]
    async fn write_json_line_emits_one_framed_write() {
        let mut writer = CountingWriter::new();
        write_json_line(&mut writer, &serde_json::json!({ "ok": true }))
            .await
            .unwrap();

        assert_eq!(writer.writes, 1);
        assert_eq!(writer.bytes, b"{\"ok\":true}\n");
    }

    #[tokio::test]
    async fn returns_invalid_data_on_bad_json() {
        let (a, b) = tokio::io::duplex(1024);
        let (mut _ar, mut aw) = tokio::io::split(a);
        let (mut br, _bw) = tokio::io::split(b);
        let mut br = BufReader::new(&mut br);

        aw.write_all(b"{not json}\n").await.unwrap();

        let err = read_json_line::<_, serde_json::Value>(&mut br)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn errors_when_line_exceeds_limit() {
        let (a, b) = tokio::io::duplex(1024 * 1024);
        let (mut _ar, mut aw) = tokio::io::split(a);
        let (mut br, _bw) = tokio::io::split(b);
        let mut br = BufReader::new(&mut br);

        // Write a line bigger than our limit.
        let big = "a".repeat(33);
        aw.write_all(big.as_bytes()).await.unwrap();
        aw.write_all(b"\n").await.unwrap();

        let err = read_json_line_with_limit::<_, serde_json::Value>(&mut br, 32)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn serve_jsonl_connection_handles_invalid_and_valid_requests() {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        struct Req {
            n: u64,
        }
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        struct Resp {
            ok: bool,
            n: u64,
        }

        let (a, b) = UnixStream::pair().unwrap();
        let h = tokio::spawn(async move {
            serve_jsonl_connection(
                a,
                |req: Req| async move { Resp { ok: true, n: req.n } },
                |_e| Resp { ok: false, n: 0 },
            )
            .await
            .unwrap();
        });

        let (r, mut w) = b.into_split();
        let mut r = BufReader::new(r);

        // Invalid JSON should yield an error response.
        w.write_all(b"{not json}\n").await.unwrap();
        let resp: Resp = read_json_line(&mut r).await.unwrap().unwrap();
        assert_eq!(resp, Resp { ok: false, n: 0 });

        // Valid JSON should roundtrip.
        write_json_line(&mut w, &Req { n: 7 }).await.unwrap();
        let resp: Resp = read_json_line(&mut r).await.unwrap().unwrap();
        assert_eq!(resp, Resp { ok: true, n: 7 });

        drop(w);
        h.await.unwrap();
    }
}
