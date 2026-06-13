use crate::release::should_use_self_signed_route_cert;
use crate::socket::Response;
use crate::tls::cloudflare::CloudflareOriginCaClient;
use crate::tls::{CertInfo, CloudflareDnsProvider, DnsBinding};
use std::time::Duration;

impl crate::ServerState {
    pub async fn request_certificate(&self, domain: &str) -> Response {
        let acme_guard = self.acme_client.read().await;
        let acme = match acme_guard.as_ref() {
            Some(acme) => acme,
            None => return Response::error("ACME is disabled".to_string()),
        };

        match acme.request_certificate(domain).await {
            Ok(cert) => Response::ok(serde_json::json!({
                "status": "issued",
                "domain": domain,
                "expires_in_days": cert.days_until_expiry(),
                "cert_path": cert.cert_path.to_string_lossy(),
            })),
            Err(e) => Response::error(format!("Certificate request failed: {}", e)),
        }
    }

    pub(crate) async fn ensure_route_certificate(
        &self,
        app_name: &str,
        domain: &str,
    ) -> Option<CertInfo> {
        if should_use_self_signed_route_cert(domain) {
            if let Some(existing) = self.cert_manager.get_cert_for_host(domain) {
                tracing::debug!(domain = %domain, "Certificate already exists");
                return Some(existing);
            }
            match self.cert_manager.get_or_create_self_signed_cert(domain) {
                Ok(cert) => {
                    tracing::info!(
                        domain = %domain,
                        app = app_name,
                        cert_path = %cert.cert_path.display(),
                        "Generated self-signed certificate for private route domain"
                    );
                    return Some(cert);
                }
                Err(e) => {
                    tracing::warn!(
                        domain = %domain,
                        app = app_name,
                        error = %e,
                        "Failed to generate self-signed certificate for private route domain"
                    );
                    return None;
                }
            }
        }

        let ssl = match self.state_store.get_ssl(app_name) {
            Ok(value) => value.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(
                    domain = %domain,
                    app = app_name,
                    error = %error,
                    "Failed to load app SSL credentials"
                );
                tako_core::SslBinding::default()
            }
        };

        if let Some(existing) = self.cert_manager.get_cert_for_host(domain)
            && self
                .cert_manager
                .cert_matches_ssl_provider(&existing, ssl.provider)
        {
            tracing::debug!(domain = %domain, "Certificate already exists");
            return Some(existing);
        }

        if ssl.provider == tako_core::SslProvider::Cloudflare {
            return self
                .request_cloudflare_origin_certificate(app_name, domain, &ssl)
                .await;
        }

        let acme_guard = self.acme_client.read().await;
        let acme = acme_guard.as_ref()?;
        let dns = cloudflare_dns_binding_from_ssl(&ssl);

        tracing::info!(domain = %domain, app = app_name, "Requesting certificate for route");
        match acme
            .request_certificate_with_dns(domain, dns.as_ref())
            .await
        {
            Ok(cert) => {
                tracing::info!(
                    domain = %domain,
                    expires_in_days = cert.days_until_expiry(),
                    "Certificate issued successfully"
                );
                Some(cert)
            }
            Err(e) => {
                tracing::warn!(
                    domain = %domain,
                    error = %e,
                    "Failed to request certificate (HTTPS may not work for this domain)"
                );
                None
            }
        }
    }

    async fn request_cloudflare_origin_certificate(
        &self,
        app_name: &str,
        domain: &str,
        ssl: &tako_core::SslBinding,
    ) -> Option<CertInfo> {
        let token = match ssl
            .cloudflare_api_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
        {
            Some(token) => token,
            None => {
                tracing::warn!(
                    domain = %domain,
                    app = app_name,
                    "Cloudflare SSL requires a Cloudflare API token"
                );
                return None;
            }
        };
        let client = match CloudflareOriginCaClient::from_api_token(token) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(
                    domain = %domain,
                    app = app_name,
                    error = %error,
                    "Failed to initialize Cloudflare Origin CA client"
                );
                return None;
            }
        };
        tracing::info!(domain = %domain, app = app_name, "Requesting Cloudflare Origin CA certificate");
        match client
            .request_certificate(domain, self.cert_manager.clone())
            .await
        {
            Ok(cert) => {
                tracing::info!(
                    domain = %domain,
                    expires_in_days = cert.days_until_expiry(),
                    "Cloudflare Origin CA certificate issued successfully"
                );
                Some(cert)
            }
            Err(error) => {
                tracing::warn!(
                    domain = %domain,
                    error = %error,
                    "Failed to request Cloudflare Origin CA certificate"
                );
                None
            }
        }
    }

    pub(crate) async fn validate_deploy_ssl_binding(
        &self,
        routes: &[String],
        ssl: &tako_core::SslBinding,
    ) -> Result<(), String> {
        if !crate::server_state::ssl_binding_needs_cloudflare_token(ssl.provider, routes) {
            return Ok(());
        }

        let token = ssl
            .cloudflare_api_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| "Cloudflare API token is missing".to_string())?;

        match ssl.provider {
            tako_core::SslProvider::LetsEncrypt => {
                let dns = CloudflareDnsProvider::from_api_token(token, Duration::ZERO)
                    .map_err(|error| error.to_string())?;
                dns.verify_wildcard_routes(routes)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            tako_core::SslProvider::Cloudflare => {
                let cloudflare = CloudflareOriginCaClient::from_api_token(token)
                    .map_err(|error| error.to_string())?;
                cloudflare
                    .verify_token()
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }

        Ok(())
    }

    pub(crate) async fn check_certificate_renewals(&self) -> Vec<Result<CertInfo, String>> {
        let acme_guard = self.acme_client.read().await;
        let acme = acme_guard.as_ref();

        let certs_to_renew = self.cert_manager.get_certs_needing_renewal();
        let mut results = Vec::new();

        for cert in certs_to_renew {
            tracing::info!(
                domain = %cert.domain,
                days_until_expiry = cert.days_until_expiry(),
                "Certificate needs renewal"
            );
            let app = {
                let route_table = self.routes.read();
                route_table.app_for_route_domain(&cert.domain)
            };
            let ssl = match app.as_deref() {
                Some(app) => self
                    .state_store
                    .get_ssl(app)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                None => tako_core::SslBinding::default(),
            };
            if ssl.provider == tako_core::SslProvider::Cloudflare {
                results.push(
                    self.request_cloudflare_origin_certificate(
                        app.as_deref().unwrap_or("unknown"),
                        &cert.domain,
                        &ssl,
                    )
                    .await
                    .ok_or_else(|| {
                        format!("Cloudflare Origin CA renewal failed for {}", cert.domain)
                    }),
                );
                continue;
            }

            let Some(acme) = acme else {
                results.push(Err("ACME is disabled".to_string()));
                continue;
            };
            let dns = cloudflare_dns_binding_from_ssl(&ssl);
            results.push(
                acme.renew_certificate_with_dns(&cert.domain, dns.as_ref())
                    .await
                    .map_err(|error| error.to_string()),
            );
        }

        results
    }
}

fn cloudflare_dns_binding_from_ssl(ssl: &tako_core::SslBinding) -> Option<DnsBinding> {
    ssl.cloudflare_api_token
        .as_ref()
        .filter(|token| !token.trim().is_empty())
        .map(|token| DnsBinding {
            cloudflare_api_token: Some(token.clone()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_dns_binding_uses_available_token_for_any_domain() {
        let ssl = tako_core::SslBinding {
            provider: tako_core::SslProvider::LetsEncrypt,
            cloudflare_api_token: Some("token".to_string()),
        };

        let binding = cloudflare_dns_binding_from_ssl(&ssl).expect("dns binding");

        assert_eq!(binding.cloudflare_api_token.as_deref(), Some("token"));
    }
}
