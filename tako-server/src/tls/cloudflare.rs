use super::manager::{CertInfo, CertManager, StoredCertIssuer};
use reqwest::header::CONTENT_TYPE;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

const CLOUDFLARE_API_BASE_URL: &str = "https://api.cloudflare.com/client/v4";
const ORIGIN_CA_REQUEST_TYPE: &str = "origin-ecc";
const ORIGIN_CA_VALIDITY_DAYS: u32 = 5475;

#[derive(Debug, Error)]
pub(crate) enum CloudflareOriginCaError {
    #[error("missing Cloudflare SSL credential {0}")]
    MissingCredential(&'static str),

    #[error("invalid Cloudflare Origin CA domain: {0}")]
    InvalidDomain(String),

    #[error("Cloudflare API error: {0}")]
    CloudflareApi(String),

    #[error("Cloudflare Origin CA HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Cloudflare Origin CA response error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("certificate request generation failed: {0}")]
    CertificateRequest(String),

    #[error("certificate error: {0}")]
    Cert(#[from] super::manager::CertError),
}

#[derive(Clone)]
pub(crate) struct CloudflareOriginCaClient {
    api_token: String,
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
pub(crate) struct CreateCertificateResponse {
    pub(crate) id: Option<String>,
    pub(crate) certificate: String,
}

impl CloudflareOriginCaClient {
    pub(crate) fn from_api_token(
        api_token: impl Into<String>,
    ) -> Result<Self, CloudflareOriginCaError> {
        Self::new(api_token.into(), CLOUDFLARE_API_BASE_URL)
    }

    fn new(
        api_token: String,
        api_base_url: impl Into<String>,
    ) -> Result<Self, CloudflareOriginCaError> {
        if api_token.trim().is_empty() {
            return Err(CloudflareOriginCaError::MissingCredential(
                "cloudflare_api_token",
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            api_token,
            api_base_url: api_base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    pub(crate) async fn request_certificate(
        &self,
        domain: &str,
        cert_manager: Arc<CertManager>,
    ) -> Result<CertInfo, CloudflareOriginCaError> {
        validate_origin_ca_domain(domain)?;
        let (csr, private_key_pem) = generate_origin_ca_csr(domain)?;
        let created = self.create_origin_certificate(domain, &csr).await?;
        tracing::info!(
            domain = domain,
            certificate_id = created.id.as_deref().unwrap_or("unknown"),
            "Cloudflare Origin CA certificate issued"
        );
        cert_manager
            .store_certificate(
                domain,
                &created.certificate,
                &private_key_pem,
                StoredCertIssuer::Cloudflare,
            )
            .map_err(CloudflareOriginCaError::Cert)
    }

    async fn create_origin_certificate(
        &self,
        domain: &str,
        csr: &str,
    ) -> Result<CreateCertificateResponse, CloudflareOriginCaError> {
        let body = serde_json::json!({
            "csr": csr,
            "hostnames": [domain],
            "request_type": ORIGIN_CA_REQUEST_TYPE,
            "requested_validity": ORIGIN_CA_VALIDITY_DAYS,
        });
        let response = self
            .http
            .post(format!("{}/certificates", self.api_base_url))
            .bearer_auth(&self.api_token)
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body)?)
            .send()
            .await?;
        let bytes = response_bytes(response).await?;
        Self::parse_create_certificate_response(&bytes)
    }

    pub(crate) async fn verify_token(&self) -> Result<(), CloudflareOriginCaError> {
        // Cloudflare's /user/tokens/verify endpoint validates user-owned API
        // tokens. Account-owned tokens (`cfat_...`) are valid Bearer tokens for
        // product APIs, but this endpoint rejects them with "Invalid API Token".
        if is_account_api_token(&self.api_token) {
            return Ok(());
        }

        let response = self
            .http
            .get(format!("{}/user/tokens/verify", self.api_base_url))
            .bearer_auth(&self.api_token)
            .send()
            .await?;
        let bytes = response_bytes(response).await?;
        let token = parse_cloudflare_response::<CloudflareTokenVerifyResult>(&bytes)?;
        if token.status != "active" {
            return Err(CloudflareOriginCaError::CloudflareApi(format!(
                "token is {}",
                token.status
            )));
        }
        Ok(())
    }

    pub(crate) fn parse_create_certificate_response(
        body: &[u8],
    ) -> Result<CreateCertificateResponse, CloudflareOriginCaError> {
        parse_cloudflare_response(body)
    }
}

fn is_account_api_token(token: &str) -> bool {
    token.trim().starts_with("cfat_")
}

fn generate_origin_ca_csr(domain: &str) -> Result<(String, String), CloudflareOriginCaError> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, domain);
    dn.push(DnType::OrganizationName, "Tako");
    params.distinguished_name = dn;
    params.subject_alt_names =
        vec![SanType::DnsName(domain.try_into().map_err(|e| {
            CloudflareOriginCaError::InvalidDomain(format!("{domain}: {e}"))
        })?)];

    let key_pair = KeyPair::generate().map_err(|error| {
        CloudflareOriginCaError::CertificateRequest(format!("failed to generate key: {error}"))
    })?;
    let csr = params
        .serialize_request(&key_pair)
        .and_then(|request| request.pem())
        .map_err(|error| {
            CloudflareOriginCaError::CertificateRequest(format!("failed to generate CSR: {error}"))
        })?;
    Ok((csr, key_pair.serialize_pem()))
}

fn validate_origin_ca_domain(domain: &str) -> Result<(), CloudflareOriginCaError> {
    let trimmed = domain.trim().trim_end_matches('.');
    if trimmed.is_empty()
        || trimmed.starts_with('.')
        || trimmed.ends_with('.')
        || trimmed.contains('/')
        || trimmed.split('.').any(str::is_empty)
    {
        return Err(CloudflareOriginCaError::InvalidDomain(domain.to_string()));
    }
    if let Some(suffix) = trimmed.strip_prefix("*.") {
        if suffix.split('.').count() < 2 || suffix.contains('*') {
            return Err(CloudflareOriginCaError::InvalidDomain(domain.to_string()));
        }
    } else if trimmed.contains('*') {
        return Err(CloudflareOriginCaError::InvalidDomain(domain.to_string()));
    }
    Ok(())
}

async fn response_bytes(
    response: reqwest::Response,
) -> Result<bytes::Bytes, CloudflareOriginCaError> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if status.is_success() {
        return Ok(bytes);
    }

    if let Ok(message) = parse_cloudflare_error_message(&bytes) {
        return Err(CloudflareOriginCaError::CloudflareApi(format!(
            "HTTP {status}: {message}"
        )));
    }

    let body = String::from_utf8_lossy(&bytes);
    Err(CloudflareOriginCaError::CloudflareApi(format!(
        "HTTP {status}: {body}"
    )))
}

fn parse_cloudflare_response<T: DeserializeOwned>(
    body: &[u8],
) -> Result<T, CloudflareOriginCaError> {
    let response: CloudflareResponse<T> = serde_json::from_slice(body)?;
    if !response.success {
        return Err(CloudflareOriginCaError::CloudflareApi(
            format_cloudflare_errors(&response.errors),
        ));
    }

    response.result.ok_or_else(|| {
        CloudflareOriginCaError::CloudflareApi("response did not include a result".to_string())
    })
}

fn parse_cloudflare_error_message(body: &[u8]) -> Result<String, CloudflareOriginCaError> {
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
            Some(1016) => format!(
                "{} (1016). Use a Cloudflare token with Zone / SSL and Certificates / Edit.",
                error.message
            ),
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
    fn parse_create_certificate_response_returns_certificate() {
        let body = br#"{
            "success": true,
            "errors": [],
            "result": {
                "id": "cert-123",
                "certificate": "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----",
                "expires_on": "2040-01-01T00:00:00Z"
            }
        }"#;

        let response = CloudflareOriginCaClient::parse_create_certificate_response(body).unwrap();

        assert_eq!(
            response.certificate,
            "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----"
        );
        assert_eq!(response.id.as_deref(), Some("cert-123"));
    }

    #[test]
    fn cloudflare_api_errors_include_message() {
        let body = br#"{
            "success": false,
            "errors": [{ "code": 10000, "message": "Authentication error" }],
            "result": null
        }"#;

        let err = CloudflareOriginCaClient::parse_create_certificate_response(body).unwrap_err();

        assert!(
            err.to_string().contains("Authentication error"),
            "error should include Cloudflare message: {err}",
        );
    }

    #[test]
    fn origin_ca_permission_errors_name_required_scope() {
        let body = br#"{
            "success": false,
            "errors": [{ "code": 1016, "message": "User is not authorized to perform this action" }],
            "result": null
        }"#;

        let err = CloudflareOriginCaClient::parse_create_certificate_response(body).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("SSL and Certificates"), "got: {message}");
        assert!(message.contains("Edit"), "got: {message}");
    }

    #[test]
    fn detects_account_api_tokens() {
        assert!(is_account_api_token("cfat_abc"));
        assert!(is_account_api_token("  cfat_abc"));
        assert!(!is_account_api_token("cfut_abc"));
        assert!(!is_account_api_token("legacy-token"));
    }

    #[tokio::test]
    async fn verify_token_skips_user_verify_for_account_api_tokens() {
        let client = CloudflareOriginCaClient::new(
            "cfat_test_account_token".to_string(),
            "http://127.0.0.1:1",
        )
        .unwrap();

        client.verify_token().await.unwrap();
    }

    #[test]
    fn origin_ca_domain_allows_leading_wildcard_only() {
        assert!(validate_origin_ca_domain("*.example.com").is_ok());
        assert!(validate_origin_ca_domain("api.example.com").is_ok());
        assert!(validate_origin_ca_domain("*.*.example.com").is_err());
        assert!(validate_origin_ca_domain("api.*.example.com").is_err());
    }
}
