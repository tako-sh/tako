use std::time::Duration;

use serde::de::DeserializeOwned;

const CLOUDFLARE_API_BASE_URL: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, thiserror::Error)]
pub(crate) enum CloudflarePreflightError {
    #[error("invalid Cloudflare domain: {0}")]
    InvalidDomain(String),
    #[error("Cloudflare token is {0}")]
    TokenStatus(String),
    #[error("Cloudflare token could not read a zone for {0}")]
    ZoneNotFound(String),
    #[error("Cloudflare API error: {0}")]
    CloudflareApi(String),
    #[error("Cloudflare HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Cloudflare response error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub(crate) struct CloudflarePreflightClient {
    api_base_url: String,
    http: reqwest::Client,
}

#[derive(Debug, serde::Deserialize)]
struct CloudflareResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CloudflareResponseError>,
    result: Option<T>,
}

#[derive(Debug, serde::Deserialize)]
struct CloudflareResponseError {
    code: Option<u64>,
    message: String,
}

#[derive(Debug, serde::Deserialize)]
struct CloudflareTokenVerifyResult {
    status: String,
}

#[derive(Debug, serde::Deserialize)]
struct CloudflareZone {
    name: String,
}

impl CloudflarePreflightClient {
    pub(crate) fn production() -> Result<Self, CloudflarePreflightError> {
        Self::new(CLOUDFLARE_API_BASE_URL)
    }

    fn new(api_base_url: impl Into<String>) -> Result<Self, CloudflarePreflightError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            api_base_url: api_base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    pub(crate) async fn verify_token(
        &self,
        api_token: &str,
    ) -> Result<(), CloudflarePreflightError> {
        let response = self
            .http
            .get(format!("{}/user/tokens/verify", self.api_base_url))
            .bearer_auth(api_token)
            .send()
            .await?;
        let bytes = response_bytes(response).await?;
        parse_token_status(&bytes)?;
        Ok(())
    }

    pub(crate) async fn verify_wildcard_zones(
        &self,
        api_token: &str,
        routes: &[String],
    ) -> Result<(), CloudflarePreflightError> {
        for route in routes.iter().filter(|route| route.starts_with("*.")) {
            self.find_zone_for_route(api_token, route).await?;
        }
        Ok(())
    }

    async fn find_zone_for_route(
        &self,
        api_token: &str,
        route: &str,
    ) -> Result<(), CloudflarePreflightError> {
        for candidate in wildcard_zone_candidates(route)? {
            let zones = self.list_zones(api_token, &candidate).await?;
            if zones.into_iter().any(|zone| zone.name == candidate) {
                return Ok(());
            }
        }

        Err(CloudflarePreflightError::ZoneNotFound(normalize_domain(
            route,
        )?))
    }

    async fn list_zones(
        &self,
        api_token: &str,
        name: &str,
    ) -> Result<Vec<CloudflareZone>, CloudflarePreflightError> {
        let mut url =
            reqwest::Url::parse(&format!("{}/zones", self.api_base_url)).map_err(|error| {
                CloudflarePreflightError::CloudflareApi(format!(
                    "invalid Cloudflare API URL: {error}"
                ))
            })?;
        url.query_pairs_mut().append_pair("name", name);
        let response = self.http.get(url).bearer_auth(api_token).send().await?;
        let bytes = response_bytes(response).await?;
        parse_cloudflare_response(&bytes)
    }
}

pub(crate) async fn preflight_ssl_cloudflare_credential(
    provider: tako_core::SslProvider,
    routes: &[String],
    api_token: &str,
) -> Result<(), CloudflarePreflightError> {
    let client = &CloudflarePreflightClient::production()?;
    client.verify_token(api_token).await?;
    if provider == tako_core::SslProvider::LetsEncrypt {
        client.verify_wildcard_zones(api_token, routes).await?;
    }
    Ok(())
}

fn wildcard_zone_candidates(route: &str) -> Result<Vec<String>, CloudflarePreflightError> {
    let normalized = normalize_domain(route)?;
    let labels: Vec<_> = normalized.split('.').collect();
    if labels.len() < 2 {
        return Err(CloudflarePreflightError::InvalidDomain(route.to_string()));
    }

    Ok((0..labels.len() - 1)
        .map(|index| labels[index..].join("."))
        .collect())
}

fn normalize_domain(domain: &str) -> Result<String, CloudflarePreflightError> {
    let host = domain
        .trim()
        .split_once('/')
        .map_or(domain.trim(), |(host, _)| host);
    let trimmed = host.trim_end_matches('.');
    let without_wildcard = trimmed.strip_prefix("*.").unwrap_or(trimmed);

    if without_wildcard.is_empty()
        || without_wildcard.starts_with('.')
        || without_wildcard.ends_with('.')
        || without_wildcard.contains('*')
        || without_wildcard.contains('/')
        || without_wildcard.split('.').any(str::is_empty)
    {
        return Err(CloudflarePreflightError::InvalidDomain(domain.to_string()));
    }

    Ok(without_wildcard.to_ascii_lowercase())
}

async fn response_bytes(
    response: reqwest::Response,
) -> Result<bytes::Bytes, CloudflarePreflightError> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if status.is_success() {
        return Ok(bytes);
    }

    if let Ok(message) = parse_cloudflare_error_message(&bytes) {
        return Err(CloudflarePreflightError::CloudflareApi(format!(
            "HTTP {status}: {message}"
        )));
    }

    let body = String::from_utf8_lossy(&bytes);
    Err(CloudflarePreflightError::CloudflareApi(format!(
        "HTTP {status}: {body}"
    )))
}

fn parse_token_status(body: &[u8]) -> Result<String, CloudflarePreflightError> {
    let token = parse_cloudflare_response::<CloudflareTokenVerifyResult>(body)?;
    if token.status == "active" {
        return Ok(token.status);
    }

    Err(CloudflarePreflightError::TokenStatus(token.status))
}

fn parse_cloudflare_response<T: DeserializeOwned>(
    body: &[u8],
) -> Result<T, CloudflarePreflightError> {
    let response: CloudflareResponse<T> = serde_json::from_slice(body)?;
    if !response.success {
        return Err(CloudflarePreflightError::CloudflareApi(
            format_cloudflare_errors(&response.errors),
        ));
    }

    response.result.ok_or_else(|| {
        CloudflarePreflightError::CloudflareApi("response did not include a result".to_string())
    })
}

fn parse_cloudflare_error_message(body: &[u8]) -> Result<String, CloudflarePreflightError> {
    let response: CloudflareResponse<serde_json::Value> = serde_json::from_slice(body)?;
    Ok(format_cloudflare_errors(&response.errors))
}

fn format_cloudflare_errors(errors: &[CloudflareResponseError]) -> String {
    if errors.is_empty() {
        return "unknown Cloudflare API error".to_string();
    }

    errors
        .iter()
        .map(|error| match error.code {
            Some(code) => format!("{} ({code})", error.message),
            None => error.message.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_zone_candidates_prefer_most_specific_zone() {
        assert_eq!(
            wildcard_zone_candidates("*.api.example.com").unwrap(),
            vec!["api.example.com".to_string(), "example.com".to_string()]
        );
    }

    #[test]
    fn wildcard_zone_candidates_ignore_route_paths() {
        assert_eq!(
            wildcard_zone_candidates("*.api.example.com/v1/*").unwrap(),
            vec!["api.example.com".to_string(), "example.com".to_string()]
        );
    }

    #[test]
    fn token_status_accepts_active_tokens() {
        let body = br#"{
            "success": true,
            "errors": [],
            "result": { "status": "active" }
        }"#;

        assert_eq!(parse_token_status(body).unwrap(), "active");
    }

    #[test]
    fn token_status_rejects_disabled_tokens() {
        let body = br#"{
            "success": true,
            "errors": [],
            "result": { "status": "disabled" }
        }"#;

        let err = parse_token_status(body).unwrap_err();

        assert!(
            err.to_string().contains("disabled"),
            "error should include token status: {err}"
        );
    }
}
