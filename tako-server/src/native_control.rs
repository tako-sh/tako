use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use tako_network::{MemberKind, NodeConfig, ServerIdentity};
use tokio::sync::Semaphore;

use crate::tls::{CertInfo, CertManager};

pub struct NativeControl {
    data_dir: PathBuf,
    hostname: String,
    requests: Semaphore,
}

impl NativeControl {
    pub fn load(
        data_dir: &Path,
        certificates: &CertManager,
    ) -> std::result::Result<Option<Self>, Box<dyn std::error::Error>> {
        match std::fs::symlink_metadata(data_dir.join("native-network.json")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        if !cfg!(target_os = "linux") {
            return Err("Native server networking requires Linux".into());
        }
        let config = NodeConfig::load(data_dir)?;
        let identity = ServerIdentity::renew(config.network_dir(data_dir))?;
        let id = identity.request().device_id_for(&config.network.id)?;
        let member = config.bootstrap.members.iter().find(|member| {
            member.device_id == id
                && member.request == *identity.request()
                && member.kind == MemberKind::Server(identity.server_info().clone())
        });
        if id != config.server_id || member.is_none() {
            return Err("Native server configuration does not match its prepared identity".into());
        }
        let hostname = format!(
            "node-{}.{}.tako.internal",
            &id.to_string()[..32],
            config.network.id
        );
        let bytes = std::fs::read(&identity.tls_paths().certificate)?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&bytes)?;
        let certificate = pem.parse_x509()?;
        let timestamp = u64::try_from(certificate.validity().not_after.timestamp())?;
        certificates.add_cert(CertInfo {
            domain: hostname.clone(),
            cert_path: identity.tls_paths().fullchain.clone(),
            key_path: identity.tls_paths().private_key.clone(),
            expires_at: Some(UNIX_EPOCH + Duration::from_secs(timestamp)),
            is_wildcard: false,
            // This issuer is local to the network; ACME must not replace it.
            is_self_signed: true,
        });
        Ok(Some(Self {
            data_dir: data_dir.into(),
            hostname,
            requests: Semaphore::new(32),
        }))
    }

    pub fn matches_host(&self, host: &str) -> bool {
        host.split(':')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(&self.hostname))
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn handle(&self, session: &mut Session) -> Result<bool> {
        reply(
            session,
            501,
            b"{\"error\":\"Native server networking requires Linux\"}",
        )
        .await
    }

    #[cfg(target_os = "linux")]
    pub async fn handle(&self, session: &mut Session) -> Result<bool> {
        if !session
            .digest()
            .is_some_and(|digest| digest.ssl_digest.is_some())
        {
            return reply(session, 403, b"{\"error\":\"TLS required\"}").await;
        }
        let path = session.req_header().uri.path().to_string();
        if path.starts_with("/v1/management/") {
            let Ok(_permit) = self.requests.try_acquire() else {
                return reply(session, 503, b"{\"error\":\"Network agent busy\"}").await;
            };
            return bridge::handle(session, &self.data_dir).await;
        }
        if !is_rpc_path(&path) || session.req_header().uri.query().is_some() {
            return reply(session, 404, b"{\"error\":\"Not found\"}").await;
        }
        if session.req_header().method.as_str() != "POST" {
            return reply(session, 405, b"{\"error\":\"POST required\"}").await;
        }
        if session
            .req_header()
            .headers
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|size| size > tako_network::api::MAX_RPC_BYTES)
        {
            return reply(session, 413, b"{\"error\":\"Request too large\"}").await;
        }
        let Ok(_permit) = self.requests.try_acquire() else {
            return reply(session, 503, b"{\"error\":\"Network agent busy\"}").await;
        };
        let body = tokio::time::timeout(Duration::from_secs(10), async {
            let mut bytes = Vec::new();
            while let Some(chunk) = session.read_request_body().await? {
                if bytes.len() + chunk.len() > tako_network::api::MAX_RPC_BYTES {
                    return Ok(None);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok::<_, Box<pingora_core::Error>>(Some(bytes))
        })
        .await;
        let bytes = match body {
            Err(_) => return reply(session, 408, b"{\"error\":\"Request timed out\"}").await,
            Ok(Err(error)) => return Err(error),
            Ok(Ok(None)) => return reply(session, 413, b"{\"error\":\"Request too large\"}").await,
            Ok(Ok(Some(bytes))) => bytes,
        };
        let result = crate::native_agent::relay(&self.data_dir, "POST", &path, &bytes).await;
        match result {
            Ok(result)
                if result.status == 200
                    && (result.ready
                        || matches!(
                            result.response,
                            Some(tako_network::api::RpcResponse::Snapshot(_))
                                | Some(tako_network::api::RpcResponse::Revoked(_))
                        )) =>
            {
                if let Some(response) = result.response {
                    match serde_json::to_vec(&response) {
                        Ok(bytes) => reply_with_readiness(session, &bytes, result.ready).await,
                        Err(_) => {
                            reply(session, 500, b"{\"error\":\"Invalid network response\"}").await
                        }
                    }
                } else {
                    reply(session, 503, b"{\"error\":\"Network not ready\"}").await
                }
            }
            Ok(result) => {
                let status = if !result.ready {
                    503
                } else if (400..600).contains(&result.status) {
                    result.status
                } else {
                    500
                };
                reply(session, status, b"{\"error\":\"Network request rejected\"}").await
            }
            Err(error) => {
                tracing::warn!(%error, "Native control request failed");
                reply(session, 503, b"{\"error\":\"Network not ready\"}").await
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod bridge {
    use super::*;
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use hyper::body::{Body, Frame};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::sync::mpsc;

    struct RequestBody(mpsc::Receiver<std::io::Result<Frame<Bytes>>>);

    impl Body for RequestBody {
        type Data = Bytes;
        type Error = std::io::Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<Frame<Bytes>, Self::Error>>> {
            self.0.poll_recv(context)
        }
    }

    pub(super) async fn handle(session: &mut Session, data_dir: &Path) -> Result<bool> {
        forward(session, |request| {
            crate::native_management_listener::relay(data_dir, request)
        })
        .await
    }

    async fn forward<F, Fut>(session: &mut Session, handler: F) -> Result<bool>
    where
        F: FnOnce(hyper::Request<RequestBody>) -> Fut,
        Fut: std::future::Future<Output = hyper::Response<http_body_util::Full<Bytes>>>,
    {
        let (sender, receiver) = mpsc::channel(4);
        let mut request = hyper::Request::new(RequestBody(receiver));
        *request.method_mut() = session.req_header().method.clone();
        *request.uri_mut() = session.req_header().uri.clone();
        *request.version_mut() = session.req_header().version;
        *request.headers_mut() = session.req_header().headers.clone();
        // The handler can reject an upload before consuming it. A closed body
        // receiver cancels the reader; this connection is never reused afterward.
        session.set_keepalive(None);
        let body_session = &mut *session;
        let pump = async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
            loop {
                let read_deadline =
                    deadline.min(tokio::time::Instant::now() + Duration::from_secs(30));
                let chunk = tokio::select! {
                    biased;
                    _ = sender.closed() => break,
                    result = tokio::time::timeout_at(read_deadline, body_session.read_request_body()) => result,
                };
                let frame = match chunk {
                    Ok(Ok(Some(chunk))) => Ok(Frame::data(chunk)),
                    Ok(Ok(None)) => break,
                    _ => Err(std::io::Error::other("management request body interrupted")),
                };
                let ended = frame.is_err();
                if !matches!(
                    tokio::time::timeout_at(deadline, sender.send(frame)).await,
                    Ok(Ok(()))
                ) || ended
                {
                    break;
                }
            }
        };
        let (_, response) = tokio::join!(pump, handler(request));
        let (parts, body) = response.into_parts();
        let body = body
            .collect()
            .await
            .expect("full response body is infallible")
            .to_bytes();
        let mut header = ResponseHeader::build(parts.status.as_u16(), None)?;
        for (name, value) in &parts.headers {
            if name != "transfer-encoding" && name != "connection" && name != "content-length" {
                header.insert_header(name.clone(), value.clone())?;
            }
        }
        header.insert_header("content-length", body.len().to_string())?;
        header.insert_header("connection", "close")?;
        header.insert_header("cache-control", "no-store")?;
        header.insert_header("x-content-type-options", "nosniff")?;
        session
            .write_response_header(Box::new(header), false)
            .await?;
        session.write_response_body(Some(body), true).await?;
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        #[tokio::test]
        async fn early_rejection_does_not_wait_for_unread_upload() {
            tokio::time::timeout(Duration::from_secs(2), async {
                let (mut client, stream) = tokio::io::duplex(8192);
                client.write_all(b"POST /v1/management/release-artifact HTTP/1.1\r\nHost: node.tako.internal\r\nContent-Length: 999999\r\n\r\n").await.unwrap();
                let mut session = Session::new_h1(Box::new(stream));
                session.read_request().await.unwrap();
                forward(&mut session, |request| async move {
                    drop(request);
                    hyper::Response::builder().status(401).body(http_body_util::Full::new(Bytes::from_static(b"denied"))).unwrap()
                }).await.unwrap();
                drop(session);
                let mut response = String::new();
                client.read_to_string(&mut response).await.unwrap();
                assert!(response.starts_with("HTTP/1.1 401"));
                assert!(response.ends_with("denied"));
                assert!(response.to_ascii_lowercase().contains("connection: close"));
            }).await.unwrap();
        }

        #[tokio::test]
        async fn chunked_request_reaches_eof_and_preserves_descriptor_headers() {
            tokio::time::timeout(Duration::from_secs(2), async {
                let (mut client, stream) = tokio::io::duplex(8192);
                client.write_all(b"POST /v1/management/rpc HTTP/1.1\r\nHost: node.tako.internal\r\nTransfer-Encoding: chunked\r\nX-Tako-Network-Authorization: signed\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").await.unwrap();
                let mut session = Session::new_h1(Box::new(stream));
                session.read_request().await.unwrap();
                forward(&mut session, |request| async move {
                    assert_eq!(request.uri().path(), "/v1/management/rpc");
                    assert_eq!(request.headers()["x-tako-network-authorization"], "signed");
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    assert_eq!(&body[..], b"abcde");
                    hyper::Response::new(http_body_util::Full::new(body))
                }).await.unwrap();
                drop(session);
                let mut response = String::new();
                client.read_to_string(&mut response).await.unwrap();
                assert!(response.starts_with("HTTP/1.1 200"));
                assert!(response.ends_with("abcde"));
            }).await.unwrap();
        }
    }
}

fn is_rpc_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/network/approve"
            | "/v1/network/begin"
            | "/v1/network/complete"
            | "/v1/network/snapshot"
            | "/v1/network/revoke"
    )
}

async fn reply(session: &mut Session, status: u16, body: &[u8]) -> Result<bool> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("content-type", "application/json")?;
    header.insert_header("content-length", body.len().to_string())?;
    header.insert_header("cache-control", "no-store")?;
    header.insert_header("x-content-type-options", "nosniff")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(body.to_vec().into()), true)
        .await?;
    Ok(true)
}

async fn reply_with_readiness(session: &mut Session, body: &[u8], ready: bool) -> Result<bool> {
    let mut header = ResponseHeader::build(200, None)?;
    header.insert_header("content-type", "application/json")?;
    header.insert_header("content-length", body.len().to_string())?;
    header.insert_header("cache-control", "no-store")?;
    header.insert_header("x-content-type-options", "nosniff")?;
    header.insert_header("x-tako-network-ready", if ready { "true" } else { "false" })?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(body.to_vec().into()), true)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn control_hostname_is_exact_and_rpc_paths_are_reserved() {
        let control = NativeControl {
            data_dir: PathBuf::new(),
            hostname: "node-abc.123.tako.internal".into(),
            requests: Semaphore::new(1),
        };
        assert!(control.matches_host("NODE-ABC.123.TAKO.INTERNAL:443"));
        for host in [
            "node-abc.123.tako.internal.example.com",
            "prefix.node-abc.123.tako.internal",
            "app.tako.internal",
        ] {
            assert!(!control.matches_host(host));
        }
        assert!(is_rpc_path("/v1/network/approve"));
        for path in ["/rpc", "/v1/network/snapshot/", "/v1/network/../snapshot"] {
            assert!(!is_rpc_path(path));
        }
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn forwarded_https_headers_cannot_expose_control_over_plain_http() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut client, stream) = tokio::io::duplex(8192);
            client.write_all(b"POST /v1/network/snapshot HTTP/1.1\r\nHost: node-abc.123.tako.internal\r\nX-Forwarded-Proto: https\r\nForwarded: proto=https\r\nContent-Length: 2\r\n\r\n{}").await.unwrap();
            let mut session = Session::new_h1(Box::new(stream));
            session.read_request().await.unwrap();
            let control = NativeControl {
                data_dir: PathBuf::from("/does-not-exist"),
                hostname: "node-abc.123.tako.internal".into(),
                requests: Semaphore::new(1),
                };
            assert!(control.handle(&mut session).await.unwrap());
            drop(session);
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 403"), "{response}");
            assert!(response.contains("TLS required"));
            assert!(!response.contains("Network not ready"));
        }).await.unwrap();
    }
}
