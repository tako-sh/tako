use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct CreatedTunnel {
    pub(super) host: String,
    pub(super) url: String,
    pub(super) session: String,
    pub(super) expires_at: u64,
}

#[derive(Debug, Deserialize)]
pub(super) struct TunnelChallenge {
    pub(super) host: String,
    pub(super) nonce: String,
}

pub(super) async fn create_tunnel_challenge(
    base_url: &str,
    app_name: &str,
    public_key: &str,
) -> Result<TunnelChallenge, String> {
    let url = format!("{}/v1/tunnel-challenges", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(TUNNEL_API_TIMEOUT)
        .timeout(TUNNEL_API_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build tunnel HTTP client: {error}"))?;
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "app": app_name,
            "public_key": public_key,
        }))
        .send()
        .await
        .map_err(|error| format!("failed to prepare tunnel: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("failed to prepare tunnel ({status}): {body}"));
    }
    response
        .json::<TunnelChallenge>()
        .await
        .map_err(|error| format!("invalid tunnel challenge: {error}"))
}

pub(super) async fn create_tunnel(
    base_url: &str,
    app_name: &str,
    public_key: &str,
    nonce: &str,
    signature: &str,
) -> Result<CreatedTunnel, String> {
    let url = format!("{}/v1/tunnels", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(TUNNEL_API_TIMEOUT)
        .timeout(TUNNEL_API_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build tunnel HTTP client: {error}"))?;
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "app": app_name,
            "public_key": public_key,
            "nonce": nonce,
            "signature": signature,
        }))
        .send()
        .await
        .map_err(|error| format!("failed to create tunnel: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("failed to create tunnel ({status}): {body}"));
    }
    response
        .json::<CreatedTunnel>()
        .await
        .map_err(|error| format!("invalid tunnel response: {error}"))
}

pub(super) fn tunnel_base_url() -> String {
    std::env::var("TAKO_TUNNEL_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TUNNEL_BASE_URL.to_string())
}

pub(super) fn tunnel_connect_url(
    base_url: &str,
    host: &str,
    session: &str,
) -> Result<String, String> {
    let trimmed = base_url.trim_end_matches('/');
    let ws_base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err("tunnel URL must start with http:// or https://".to_string());
    };
    let mut url = reqwest::Url::parse(&format!("{ws_base}/v1/tunnels/connect"))
        .map_err(|error| format!("invalid tunnel URL: {error}"))?;
    url.query_pairs_mut()
        .append_pair("host", host)
        .append_pair("session", session);
    Ok(url.to_string())
}
