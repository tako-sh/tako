use super::super::INTERNAL_TOKEN_HEADER;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;

pub(super) async fn probe_endpoint_tcp(
    endpoint: SocketAddr,
    health_check_path: &str,
    health_check_host: &str,
    internal_token: Option<&str>,
    probe_timeout: Duration,
) -> Result<bool, std::io::Error> {
    use tokio::io::AsyncWriteExt;

    let mut socket = match timeout(probe_timeout, tokio::net::TcpStream::connect(endpoint)).await {
        Ok(result) => result?,
        Err(_) => return Ok(false),
    };
    let token_header = internal_token
        .map(|token| format!("{INTERNAL_TOKEN_HEADER}: {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET {health_check_path} HTTP/1.1\r\nHost: {health_check_host}\r\n{token_header}Connection: close\r\n\r\n"
    );
    match timeout(probe_timeout, socket.write_all(request.as_bytes())).await {
        Ok(result) => result?,
        Err(_) => return Ok(false),
    }

    let Some(response) = read_http_response_headers(&mut socket, probe_timeout).await? else {
        return Ok(false);
    };
    Ok(http_response_is_success(&response, internal_token))
}

const MAX_HEALTH_RESPONSE_BYTES: usize = 4096;

async fn read_http_response_headers(
    socket: &mut tokio::net::TcpStream,
    io_timeout: Duration,
) -> Result<Option<String>, std::io::Error> {
    use tokio::io::AsyncReadExt;

    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    loop {
        let bytes_read = match timeout(io_timeout, socket.read(&mut chunk)).await {
            Ok(result) => result?,
            Err(_) => return Ok(None),
        };

        if bytes_read == 0 {
            break;
        }

        response.extend_from_slice(&chunk[..bytes_read]);
        if response.len() > MAX_HEALTH_RESPONSE_BYTES {
            return Ok(None);
        }
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    if response.is_empty() {
        return Ok(None);
    }

    Ok(Some(String::from_utf8_lossy(&response).into_owned()))
}

fn http_status_is_success(status_line: &str) -> bool {
    let mut parts = status_line.split_whitespace();
    let Some(http_version) = parts.next() else {
        return false;
    };
    if !http_version.starts_with("HTTP/") {
        return false;
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .map(|code| (200..300).contains(&code))
        .unwrap_or(false)
}

fn http_response_is_success(response: &str, expected_token: Option<&str>) -> bool {
    let mut lines = response.lines();
    let status_line = lines.next().unwrap_or_default();
    if !http_status_is_success(status_line) {
        return false;
    }
    let Some(expected_token) = expected_token else {
        return true;
    };

    lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case(INTERNAL_TOKEN_HEADER) && value.trim() == expected_token
        })
}
