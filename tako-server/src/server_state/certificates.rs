use super::*;

impl ServerState {
    pub(crate) async fn stage_prepared_deploy_ssl(
        &self,
        app: &str,
        path: &std::path::Path,
        routes: Vec<String>,
        ssl: tako_core::SslBinding,
    ) {
        self.prepared_deploy_ssl.write().await.insert(
            PreparedDeployKey {
                app: app.to_string(),
                path: path.to_path_buf(),
            },
            PreparedDeploySsl { routes, ssl },
        );
    }

    pub(crate) async fn clear_prepared_deploy_ssl(&self, app: &str, path: &std::path::Path) {
        self.prepared_deploy_ssl
            .write()
            .await
            .remove(&PreparedDeployKey {
                app: app.to_string(),
                path: path.to_path_buf(),
            });
    }

    pub(crate) async fn resolve_deploy_ssl_binding(
        &self,
        app: &str,
        path: &std::path::Path,
        routes: &[String],
        ssl: tako_core::SslBinding,
    ) -> Result<tako_core::SslBinding, String> {
        if !ssl_binding_needs_cloudflare_token(ssl.provider, routes) {
            self.clear_prepared_deploy_ssl(app, path).await;
            return Ok(ssl);
        }

        if ssl
            .cloudflare_api_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
        {
            self.clear_prepared_deploy_ssl(app, path).await;
            return Ok(ssl);
        }

        let prepared = self
            .prepared_deploy_ssl
            .write()
            .await
            .remove(&PreparedDeployKey {
                app: app.to_string(),
                path: path.to_path_buf(),
            })
            .ok_or_else(|| {
                "Prepared SSL credentials are missing. Re-run `tako deploy`.".to_string()
            })?;

        if prepared.routes != routes {
            return Err(
                "Prepared SSL credentials do not match the deploy routes. Re-run `tako deploy`."
                    .to_string(),
            );
        }
        if prepared.ssl.provider != ssl.provider {
            return Err(
                "Prepared SSL credentials do not match the deploy SSL provider. Re-run `tako deploy`."
                    .to_string(),
            );
        }
        if prepared
            .ssl
            .cloudflare_api_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
        {
            return Err(
                "Prepared SSL credentials did not include a Cloudflare API token. Re-run `tako deploy`."
                    .to_string(),
            );
        }

        Ok(prepared.ssl)
    }
}

pub(crate) fn ssl_binding_needs_cloudflare_token(
    provider: tako_core::SslProvider,
    routes: &[String],
) -> bool {
    provider == tako_core::SslProvider::Cloudflare
        || (provider == tako_core::SslProvider::LetsEncrypt
            && routes.iter().any(|route| route.starts_with("*.")))
}
