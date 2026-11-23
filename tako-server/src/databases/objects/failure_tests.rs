use super::*;
use std::io::{Read, Write};
use std::time::Duration;

/// Serve `requests` GET requests from `objects`, keyed by object key; any
/// other key is 404. Returns the storage endpoint.
fn object_fixture(
    objects: std::collections::HashMap<String, Vec<u8>>,
    requests: usize,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let (listener, endpoint, acceptor) = tls_listener();
    let fixture = tokio::task::spawn_blocking(move || {
        let mut served = Vec::new();
        for _ in 0..requests {
            let mut stream = accept(&listener, &acceptor);
            let request = read_request(&mut stream);
            let line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            let mut parts = line.split(' ');
            let method = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();
            let key = path
                .split('?')
                .next()
                .unwrap_or_default()
                .trim_start_matches("/fixture/");
            served.push(format!("{method} {key}"));
            match objects.get(key).filter(|_| method == "GET") {
                Some(body) => {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                }
                None => write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap(),
            }
        }
        served
    });
    (endpoint, fixture)
}

/// A test-only certificate authority and the 127.0.0.1 certificate it
/// issued, shared by every storage fixture in the process.
struct FixtureTls {
    ca_pem: String,
    cert_pem: String,
    key_pem: String,
}

fn fixture_tls() -> &'static FixtureTls {
    static TLS: std::sync::OnceLock<FixtureTls> = std::sync::OnceLock::new();
    TLS.get_or_init(|| {
        let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Tako backup storage fixture CA");
        let ca =
            rcgen::CertifiedIssuer::self_signed(ca_params, rcgen::KeyPair::generate().unwrap())
                .unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".into()])
            .unwrap()
            .signed_by(&key, &ca)
            .unwrap();
        FixtureTls {
            ca_pem: ca.pem(),
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        }
    })
}

/// The authority behind every storage fixture's certificate. Unit-test
/// clients trust it; certificate validation stays on.
pub(super) fn fixture_store_certificate() -> object_store::Certificate {
    object_store::Certificate::from_pem(fixture_tls().ca_pem.as_bytes()).unwrap()
}

pub(super) fn tls_listener() -> (std::net::TcpListener, String, openssl::ssl::SslAcceptor) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("https://{}", listener.local_addr().unwrap());
    let tls = fixture_tls();
    let mut acceptor =
        openssl::ssl::SslAcceptor::mozilla_intermediate(openssl::ssl::SslMethod::tls()).unwrap();
    acceptor
        .set_certificate(&openssl::x509::X509::from_pem(tls.cert_pem.as_bytes()).unwrap())
        .unwrap();
    acceptor
        .set_private_key(
            &openssl::pkey::PKey::private_key_from_pem(tls.key_pem.as_bytes()).unwrap(),
        )
        .unwrap();
    (listener, endpoint, acceptor.build())
}

pub(super) fn accept(
    listener: &std::net::TcpListener,
    acceptor: &openssl::ssl::SslAcceptor,
) -> openssl::ssl::SslStream<std::net::TcpStream> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "storage fixture timed out"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    acceptor.accept(stream).unwrap()
}

/// Read one whole request, body included. Answering and closing while the
/// client is still sending its body resets the connection, and the client
/// then reports a transport error instead of the fixture's response.
pub(super) fn read_request(stream: &mut impl Read) -> Vec<u8> {
    fn fill(stream: &mut impl Read, bytes: &mut Vec<u8>) {
        let mut chunk = [0; 4096];
        let size = stream.read(&mut chunk).unwrap();
        assert_ne!(size, 0, "request ended early");
        bytes.extend_from_slice(&chunk[..size]);
    }
    /// The index just past the next CRLF at or after `from`.
    fn line_end(stream: &mut impl Read, bytes: &mut Vec<u8>, from: usize) -> usize {
        loop {
            if let Some(offset) = bytes[from..].windows(2).position(|value| value == b"\r\n") {
                return from + offset + 2;
            }
            fill(stream, bytes);
        }
    }

    let mut bytes = Vec::new();
    let head_end = loop {
        if let Some(offset) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            break offset + 4;
        }
        fill(stream, &mut bytes);
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]).to_ascii_lowercase();
    let header = |name: &str| {
        head.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|value| value.trim().to_string())
    };
    if let Some(length) = header("content-length:") {
        let end = head_end + length.parse::<usize>().unwrap();
        while bytes.len() < end {
            fill(stream, &mut bytes);
        }
    } else if header("transfer-encoding:").is_some_and(|value| value.contains("chunked")) {
        let mut cursor = head_end;
        loop {
            let size_end = line_end(stream, &mut bytes, cursor);
            let size_line = String::from_utf8_lossy(&bytes[cursor..size_end - 2]).to_string();
            let size = size_line.split(';').next().unwrap().trim();
            let size = usize::from_str_radix(size, 16).unwrap();
            if size == 0 {
                // Trailers, if any, end with an empty line.
                cursor = size_end;
                loop {
                    let end = line_end(stream, &mut bytes, cursor);
                    let empty = end - cursor == 2;
                    cursor = end;
                    if empty {
                        break;
                    }
                }
                break;
            }
            cursor = size_end + size + 2;
            while bytes.len() < cursor {
                fill(stream, &mut bytes);
            }
        }
    }
    bytes
}

#[test]
fn the_fixture_reads_a_request_to_the_end_of_its_body() {
    // One byte per read, so every boundary falls between reads.
    struct Trickle<'a>(&'a [u8]);
    impl Read for Trickle<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let size = self.0.len().min(1).min(buffer.len());
            buffer[..size].copy_from_slice(&self.0[..size]);
            self.0 = &self.0[size..];
            Ok(size)
        }
    }
    for request in [
        &b"GET /fixture/key HTTP/1.1\r\nHost: fixture\r\n\r\n"[..],
        b"PUT /fixture/key HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello",
        b"PUT /fixture/key HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
          5\r\n0\r\n\r\n\r\nA;ext=1\r\n0123456789\r\n0\r\n\r\n",
        b"PUT /fixture/key HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n\
          3\r\nabc\r\n0\r\nTrailer: value\r\n\r\n",
    ] {
        let mut stream = Trickle(request);
        assert_eq!(read_request(&mut stream), request);
        assert!(stream.0.is_empty());
    }
}

pub(super) fn storage_binding(endpoint: &str) -> tako_core::StorageBinding {
    serde_json::from_value(serde_json::json!({
        "provider": "s3", "bucket": "fixture", "endpoint": endpoint,
        "region": "local", "access_key_id": "fixture", "secret_access_key": "fixture",
        "force_path_style": true
    }))
    .unwrap()
}

#[tokio::test]
async fn backup_clients_validate_the_storage_certificate() {
    // Trusted: the fixture's authority issued its certificate for 127.0.0.1.
    let (endpoint, fixture) = object_fixture(Default::default(), 1);
    let result = backup_store(&storage_binding(&endpoint))
        .unwrap()
        .get(&ObjectPath::from("missing"))
        .await;
    assert!(matches!(result, Err(object_store::Error::NotFound { .. })));
    fixture.await.unwrap();

    // Not trusted: a client without that authority refuses the same server.
    let (listener, endpoint, acceptor) = tls_listener();
    let server = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    assert!(acceptor.accept(stream).is_err());
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
    });
    let result = tako_storage::s3(&storage_binding(&endpoint))
        .unwrap()
        .get(&ObjectPath::from("missing"))
        .await;
    assert!(result.is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn backup_request_deadline_covers_a_stalled_response_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let fixture = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\npartial")
            .await
            .unwrap();
        let _ = stream.read(&mut [0]).await;
    });
    let store = tako_storage::s3_builder(&storage_binding("https://localhost"))
        .unwrap()
        .with_endpoint(url)
        .with_allow_http(true)
        .build()
        .unwrap();
    let error = with_deadline(Duration::from_millis(100), async {
        let result = store
            .get(&ObjectPath::from("stalled"))
            .await
            .map_err(|e| e.to_string())?;
        result.bytes().await.map_err(|e| e.to_string())
    })
    .await
    .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    fixture.abort();
}
