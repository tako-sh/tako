//! Shared transport for calls to an app's internal primitive dispatcher.

use std::sync::OnceLock;
use std::time::Duration;
use tako_core::{ErrorCode, Response, SocketError};

pub async fn invoke_http(
    url: &str,
    host: &str,
    token: Option<&str>,
    invocation: &(impl serde::Serialize + ?Sized),
) -> Response {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(25))
            .build()
            .expect("internal HTTP client")
    });
    let mut request = client.post(url).header("Host", host).json(invocation);
    if let Some(token) = token {
        request = request.header("x-tako-internal-token", token);
    }
    let result = async {
        let mut response = request
            .send()
            .await
            // Transport failures, not an answer: the caller may retry.
            .map_err(|_| SocketError::new(ErrorCode::Unavailable, "destination unavailable"))?;
        if let Some(token) = token {
            if response
                .headers()
                .get("x-tako-internal-token")
                .and_then(|v| v.to_str().ok())
                != Some(token)
            {
                return Err("destination did not authenticate its response".into());
            }
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| SocketError::new(ErrorCode::Unavailable, "destination response failed"))?
        {
            if body.len() + chunk.len() > 1024 * 1024 {
                return Err("destination response too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| SocketError::from("invalid destination response"))
    }
    .await;
    result.unwrap_or_else(Response::from)
}
