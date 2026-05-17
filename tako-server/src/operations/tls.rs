use crate::release::should_use_self_signed_route_cert;
use crate::socket::Response;
use crate::tls::{AcmeError, CertInfo};

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
        if let Some(existing) = self.cert_manager.get_cert_for_host(domain) {
            tracing::debug!(domain = %domain, "Certificate already exists");
            return Some(existing);
        }

        if should_use_self_signed_route_cert(domain) {
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

        let acme_guard = self.acme_client.read().await;
        let acme = acme_guard.as_ref()?;
        let dns = if domain.starts_with("*.") {
            match self.state_store.get_dns(app_name) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        domain = %domain,
                        app = app_name,
                        error = %error,
                        "Failed to load app DNS credentials"
                    );
                    None
                }
            }
        } else {
            None
        };

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

    pub(crate) async fn check_certificate_renewals(&self) -> Vec<Result<CertInfo, AcmeError>> {
        let acme_guard = self.acme_client.read().await;
        let Some(acme) = acme_guard.as_ref() else {
            return Vec::new();
        };

        let certs_to_renew = self.cert_manager.get_certs_needing_renewal();
        let mut results = Vec::new();

        for cert in certs_to_renew {
            tracing::info!(
                domain = %cert.domain,
                days_until_expiry = cert.days_until_expiry(),
                "Certificate needs renewal"
            );
            let dns = if cert.domain.starts_with("*.") {
                let app = {
                    let route_table = self.routes.read().await;
                    route_table.app_for_route_domain(&cert.domain)
                };
                match app {
                    Some(app) => match self.state_store.get_dns(&app) {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::warn!(
                                app = %app,
                                domain = %cert.domain,
                                error = %error,
                                "Failed to load app DNS credentials for certificate renewal"
                            );
                            None
                        }
                    },
                    None => None,
                }
            } else {
                None
            };
            let result = acme
                .renew_certificate_with_dns(&cert.domain, dns.as_ref())
                .await;
            results.push(result);
        }

        results
    }
}
