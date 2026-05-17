use async_trait::async_trait;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::CONTENT_TYPE;
use serde::de::DeserializeOwned;
use std::time::Duration;
use thiserror::Error;

pub(crate) const CLOUDFLARE_DNS_PROVIDER: &str = "cloudflare";
const CLOUDFLARE_API_BASE_URL: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, Error)]
pub(crate) enum DnsError {
    #[error("missing DNS credential {0}")]
    MissingCredential(&'static str),

    #[error("invalid DNS challenge domain: {0}")]
    InvalidDomain(String),

    #[error("no Cloudflare zone found for {0}")]
    ZoneNotFound(String),

    #[error("Cloudflare API error: {0}")]
    CloudflareApi(String),

    #[error("Cloudflare HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Cloudflare response error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub(crate) struct DnsChallengeRecord {
    pub(crate) zone_id: String,
    pub(crate) record_id: String,
    pub(crate) name: String,
}

#[async_trait]
pub(crate) trait DnsChallengeProvider: Send + Sync {
    async fn present(&self, domain: &str, value: &str) -> Result<DnsChallengeRecord, DnsError>;
    async fn cleanup(&self, record: &DnsChallengeRecord) -> Result<(), DnsError>;
    async fn wait_for_propagation(&self) -> Result<(), DnsError>;
}

#[derive(Clone)]
pub(crate) struct CloudflareDnsProvider {
    api_token: String,
    api_base_url: String,
    propagation_delay: Duration,
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

#[derive(Debug, Clone, serde::Deserialize)]
struct CloudflareZone {
    id: String,
    name: String,
}

#[derive(Debug, serde::Deserialize)]
struct CloudflareDnsRecord {
    id: String,
}

impl CloudflareDnsProvider {
    pub(crate) fn from_api_token(
        api_token: impl Into<String>,
        propagation_delay: Duration,
    ) -> Result<Self, DnsError> {
        Self::new(api_token.into(), CLOUDFLARE_API_BASE_URL, propagation_delay)
    }

    fn new(
        api_token: String,
        api_base_url: impl Into<String>,
        propagation_delay: Duration,
    ) -> Result<Self, DnsError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            api_token,
            api_base_url: api_base_url.into().trim_end_matches('/').to_string(),
            propagation_delay,
            http,
        })
    }

    pub(crate) fn challenge_name(domain: &str) -> Result<String, DnsError> {
        Ok(format!("_acme-challenge.{}", normalize_domain(domain)?))
    }

    pub(crate) fn zone_candidates(domain: &str) -> Result<Vec<String>, DnsError> {
        let normalized = normalize_domain(domain)?;
        let labels: Vec<_> = normalized.split('.').collect();
        if labels.len() < 2 {
            return Err(DnsError::InvalidDomain(domain.to_string()));
        }

        Ok((0..labels.len() - 1)
            .map(|index| labels[index..].join("."))
            .collect())
    }

    fn parse_create_record_response(body: &[u8]) -> Result<String, DnsError> {
        Ok(parse_cloudflare_response::<CloudflareDnsRecord>(body)?.id)
    }

    async fn find_zone(&self, domain: &str) -> Result<CloudflareZone, DnsError> {
        for candidate in Self::zone_candidates(domain)? {
            let zones = self.list_zones(&candidate).await?;
            if let Some(zone) = zones.into_iter().find(|zone| zone.name == candidate) {
                return Ok(zone);
            }
        }

        Err(DnsError::ZoneNotFound(normalize_domain(domain)?))
    }

    async fn list_zones(&self, name: &str) -> Result<Vec<CloudflareZone>, DnsError> {
        let encoded_name = utf8_percent_encode(name, NON_ALPHANUMERIC);
        let response = self
            .http
            .get(format!("{}/zones?name={encoded_name}", self.api_base_url))
            .bearer_auth(&self.api_token)
            .send()
            .await?;
        self.parse_response(response).await
    }

    async fn create_txt_record(
        &self,
        zone_id: &str,
        name: &str,
        value: &str,
    ) -> Result<String, DnsError> {
        let body = serde_json::json!({
            "type": "TXT",
            "name": name,
            "content": value,
            "ttl": 60,
        });
        let response = self
            .http
            .post(format!("{}/zones/{zone_id}/dns_records", self.api_base_url))
            .bearer_auth(&self.api_token)
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body)?)
            .send()
            .await?;
        let bytes = response_bytes(response).await?;
        Self::parse_create_record_response(&bytes)
    }

    async fn delete_txt_record(&self, zone_id: &str, record_id: &str) -> Result<(), DnsError> {
        let response = self
            .http
            .delete(format!(
                "{}/zones/{zone_id}/dns_records/{record_id}",
                self.api_base_url
            ))
            .bearer_auth(&self.api_token)
            .send()
            .await?;
        let bytes = response_bytes(response).await?;
        let _ = parse_cloudflare_response::<serde_json::Value>(&bytes)?;
        Ok(())
    }

    async fn parse_response<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, DnsError> {
        let bytes = response_bytes(response).await?;
        parse_cloudflare_response(&bytes)
    }
}

#[async_trait]
impl DnsChallengeProvider for CloudflareDnsProvider {
    async fn present(&self, domain: &str, value: &str) -> Result<DnsChallengeRecord, DnsError> {
        let zone = self.find_zone(domain).await?;
        let name = Self::challenge_name(domain)?;
        let record_id = self.create_txt_record(&zone.id, &name, value).await?;

        Ok(DnsChallengeRecord {
            zone_id: zone.id,
            record_id,
            name,
        })
    }

    async fn cleanup(&self, record: &DnsChallengeRecord) -> Result<(), DnsError> {
        self.delete_txt_record(&record.zone_id, &record.record_id)
            .await
    }

    async fn wait_for_propagation(&self) -> Result<(), DnsError> {
        tokio::time::sleep(self.propagation_delay).await;
        Ok(())
    }
}

fn normalize_domain(domain: &str) -> Result<String, DnsError> {
    let trimmed = domain.trim().trim_end_matches('.');
    let without_wildcard = trimmed.strip_prefix("*.").unwrap_or(trimmed);

    if without_wildcard.is_empty()
        || without_wildcard.starts_with('.')
        || without_wildcard.ends_with('.')
        || without_wildcard.contains('*')
        || without_wildcard.contains('/')
        || without_wildcard.split('.').any(str::is_empty)
    {
        return Err(DnsError::InvalidDomain(domain.to_string()));
    }

    Ok(without_wildcard.to_ascii_lowercase())
}

async fn response_bytes(response: reqwest::Response) -> Result<bytes::Bytes, DnsError> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if status.is_success() {
        return Ok(bytes);
    }

    if let Ok(message) = parse_cloudflare_error_message(&bytes) {
        return Err(DnsError::CloudflareApi(format!("HTTP {status}: {message}")));
    }

    let body = String::from_utf8_lossy(&bytes);
    Err(DnsError::CloudflareApi(format!("HTTP {status}: {body}")))
}

fn parse_cloudflare_response<T: DeserializeOwned>(body: &[u8]) -> Result<T, DnsError> {
    let response: CloudflareResponse<T> = serde_json::from_slice(body)?;
    if !response.success {
        return Err(DnsError::CloudflareApi(format_cloudflare_errors(
            &response.errors,
        )));
    }

    response
        .result
        .ok_or_else(|| DnsError::CloudflareApi("response did not include a result".to_string()))
}

fn parse_cloudflare_error_message(body: &[u8]) -> Result<String, DnsError> {
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
    fn cloudflare_challenge_name_strips_wildcard_prefix() {
        assert_eq!(
            CloudflareDnsProvider::challenge_name("*.example.com").unwrap(),
            "_acme-challenge.example.com",
        );
        assert_eq!(
            CloudflareDnsProvider::challenge_name("*.api.example.com").unwrap(),
            "_acme-challenge.api.example.com",
        );
    }

    #[test]
    fn cloudflare_zone_candidates_prefer_most_specific_zone() {
        assert_eq!(
            CloudflareDnsProvider::zone_candidates("*.api.example.com").unwrap(),
            vec!["api.example.com".to_string(), "example.com".to_string()],
        );
    }

    #[test]
    fn cloudflare_challenge_name_rejects_invalid_wildcard_domain() {
        assert!(CloudflareDnsProvider::challenge_name("*.*.example.com").is_err());
    }

    #[test]
    fn cloudflare_create_record_response_returns_record_id() {
        let body = br#"{
            "success": true,
            "errors": [],
            "result": { "id": "record-123" }
        }"#;

        assert_eq!(
            CloudflareDnsProvider::parse_create_record_response(body).unwrap(),
            "record-123",
        );
    }

    #[test]
    fn cloudflare_api_errors_include_cloudflare_message() {
        let body = br#"{
            "success": false,
            "errors": [{ "code": 9109, "message": "Invalid access token" }],
            "result": null
        }"#;

        let err = CloudflareDnsProvider::parse_create_record_response(body).unwrap_err();

        assert!(
            err.to_string().contains("Invalid access token"),
            "error should include Cloudflare message: {err}",
        );
    }
}
