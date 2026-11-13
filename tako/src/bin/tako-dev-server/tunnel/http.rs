use super::*;

pub(super) fn local_proxy_client(
    local_host: &str,
    listen_addr: &str,
) -> Result<reqwest::Client, String> {
    local_proxy_client_with_trusted_root(local_host, listen_addr, None)
}

#[cfg(test)]
pub(super) fn local_proxy_client_with_root(
    local_host: &str,
    listen_addr: &str,
    trusted_root_pem: &str,
) -> Result<reqwest::Client, String> {
    local_proxy_client_with_trusted_root(local_host, listen_addr, Some(trusted_root_pem))
}

pub(super) fn local_proxy_client_with_trusted_root(
    local_host: &str,
    listen_addr: &str,
    trusted_root_pem: Option<&str>,
) -> Result<reqwest::Client, String> {
    let listen_addr = local_proxy_listen_addr(listen_addr)?;
    if !listen_addr.ip().is_loopback() {
        return Err("tunnel forwarding requires a loopback proxy address".to_string());
    }
    let mut client_builder = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(local_host, &[listen_addr]);
    if let Some(pem) = trusted_root_pem {
        let certificate = reqwest::Certificate::from_pem(pem.as_bytes())
            .map_err(|error| format!("invalid tunnel local proxy root certificate: {error}"))?;
        client_builder = client_builder.add_root_certificate(certificate);
    }
    client_builder
        .build()
        .map_err(|error| format!("failed to build tunnel HTTP client: {error}"))
}

pub(super) async fn send_error_response(
    outbound: &mpsc::Sender<Vec<u8>>,
    id: &str,
    status: u16,
    error: &str,
    credit: Option<&ResponseCredit>,
) -> Result<(), String> {
    send_client_message(
        outbound,
        ClientMessage::Response(TunnelResponseMessage {
            id: id.into(),
            status,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
        }),
    )
    .await?;
    let mut end = error.len().min(CHUNK_SIZE);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    let data = error.as_bytes()[..end].to_vec();
    if let Some(credit) = credit {
        credit.consume(data.len()).await?;
    }
    send_client_message(
        outbound,
        ClientMessage::ResponseChunk(TunnelBodyChunk {
            id: id.into(),
            data,
        }),
    )
    .await?;
    send_client_message(outbound, ClientMessage::ResponseEnd { id: id.into() }).await
}

pub(super) async fn forward_to_local_proxy(
    client: &reqwest::Client,
    snapshot: &TunnelSnapshot,
    request: TunnelRequestMessage,
    outbound: &mpsc::Sender<Vec<u8>>,
    credit: &ResponseCredit,
) -> Result<(), String> {
    let id = request.id.clone();
    let response = match forward_to_local_proxy_inner(client, snapshot, request).await {
        Ok(response) => response,
        Err(error) => return send_error_response(outbound, &id, 502, &error, Some(credit)).await,
    };
    send_client_message(
        outbound,
        ClientMessage::Response(TunnelResponseMessage {
            id: id.clone(),
            status: response.status().as_u16(),
            headers: forwarded_response_headers(response.headers()),
        }),
    )
    .await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return send_client_message(outbound, ClientMessage::ResponseError { id }).await;
            }
        };
        for data in chunk.chunks(CHUNK_SIZE) {
            credit.consume(data.len()).await?;
            send_client_message(
                outbound,
                ClientMessage::ResponseChunk(TunnelBodyChunk {
                    id: id.clone(),
                    data: data.to_vec(),
                }),
            )
            .await?;
        }
    }
    send_client_message(outbound, ClientMessage::ResponseEnd { id }).await
}

pub(super) async fn forward_to_local_proxy_inner(
    client: &reqwest::Client,
    snapshot: &TunnelSnapshot,
    request: TunnelRequestMessage,
) -> Result<reqwest::Response, String> {
    let method = request
        .method
        .parse::<reqwest::Method>()
        .map_err(|error| format!("invalid method: {error}"))?;
    let url = local_proxy_url(&snapshot.local_host, &snapshot.listen_addr, &request.path)?;
    let mut builder = client
        .request(method, url)
        .header(reqwest::header::HOST, snapshot.local_host.as_str());
    for (name, value) in forwarded_request_headers(&request.headers) {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(&value) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    // Only establishing the response has a deadline; SSE may live indefinitely.
    tokio::time::timeout(Duration::from_secs(30), builder.body(request.body).send())
        .await
        .map_err(|_| "local proxy response headers timed out".to_string())?
        .map_err(|error| format!("local proxy request failed: {error}"))
}

pub(super) fn local_proxy_url(
    local_host: &str,
    listen_addr: &str,
    path: &str,
) -> Result<String, String> {
    let listen_addr = local_proxy_listen_addr(listen_addr)?;
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(format!(
        "https://{}:{}{}",
        local_host,
        listen_addr.port(),
        path
    ))
}

pub(super) fn local_proxy_listen_addr(listen_addr: &str) -> Result<SocketAddr, String> {
    listen_addr
        .parse()
        .map_err(|error| format!("invalid local proxy listen address: {error}"))
}

pub(super) fn forwarded_request_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop_or_host(name))
        .cloned()
        .collect()
}

pub(super) fn forwarded_response_headers(
    headers: &reqwest::header::HeaderMap,
) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            if is_hop_by_hop_or_host(name.as_str()) || name == reqwest::header::CONTENT_LENGTH {
                return None;
            }
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect()
}
