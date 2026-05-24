use crate::config::SSL_CLOUDFLARE_CREDENTIAL_NAME;
use crate::config::SecretsStore;

use super::{SECRET_EXPIRY_WARNING_DAYS, ValidationResult};

pub fn validate_ssl_for_deployment(
    routes: &[String],
    ssl_provider: tako_core::SslProvider,
    secrets: &SecretsStore,
    env_name: &str,
) -> ValidationResult {
    let mut result = ValidationResult::new();
    if ssl_provider == tako_core::SslProvider::LetsEncrypt
        && !letsencrypt_routes_need_cloudflare_token(routes)
    {
        return result;
    }

    let Some(credential) = secrets.get_credential(env_name, SSL_CLOUDFLARE_CREDENTIAL_NAME) else {
        result.error(
            crate::commands::credentials::missing_ssl_cloudflare_credential_message(
                env_name,
                ssl_provider,
            ),
        );
        return result;
    };

    match credential.is_expired() {
        Ok(true) => {
            if let Some(expires_on) = &credential.expires_on {
                result.error(format!(
                    "Credential {SSL_CLOUDFLARE_CREDENTIAL_NAME} for environment '{env_name}' expired on {expires_on}. Run `tako credentials set {SSL_CLOUDFLARE_CREDENTIAL_NAME} --env {env_name}` to update it."
                ));
            }
        }
        Ok(false) => match credential.is_expiring_within_days(SECRET_EXPIRY_WARNING_DAYS) {
            Ok(true) => {
                if let Some(expires_on) = &credential.expires_on {
                    result.warn(format!(
                        "Credential {SSL_CLOUDFLARE_CREDENTIAL_NAME} for environment '{env_name}' expires within {SECRET_EXPIRY_WARNING_DAYS} days on {expires_on}. Run `tako credentials set {SSL_CLOUDFLARE_CREDENTIAL_NAME} --env {env_name}` to rotate it."
                    ));
                }
            }
            Ok(false) => {}
            Err(error) => result.error(format!(
                "Credential {SSL_CLOUDFLARE_CREDENTIAL_NAME} for environment '{env_name}' has invalid expiry metadata: {error}"
            )),
        },
        Err(error) => result.error(format!(
            "Credential {SSL_CLOUDFLARE_CREDENTIAL_NAME} for environment '{env_name}' has invalid expiry metadata: {error}"
        )),
    }

    result
}

pub(crate) fn letsencrypt_routes_need_cloudflare_token(routes: &[String]) -> bool {
    routes.iter().any(|route| route.starts_with("*."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Duration, OffsetDateTime};

    fn future_expiry(days: i64) -> String {
        let expires_on = OffsetDateTime::now_utc() + Duration::days(days);
        format!(
            "{:04}-{:02}-{:02}",
            expires_on.year(),
            u8::from(expires_on.month()),
            expires_on.day()
        )
    }

    #[test]
    fn cloudflare_ssl_requires_credentials() {
        let result = validate_ssl_for_deployment(
            &["app.example.com".to_string()],
            tako_core::SslProvider::Cloudflare,
            &SecretsStore::default(),
            "production",
        );

        assert!(result.has_errors());
        assert!(
            result.errors[0].contains(
                "Cloudflare SSL requires credential ssl.cloudflare. Run `tako credentials set ssl.cloudflare --env production`."
            ),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn letsencrypt_exact_routes_do_not_require_credentials() {
        let result = validate_ssl_for_deployment(
            &["app.example.com".to_string()],
            tako_core::SslProvider::LetsEncrypt,
            &SecretsStore::default(),
            "production",
        );

        assert!(!result.has_errors(), "{:?}", result.errors);
    }

    #[test]
    fn letsencrypt_wildcard_routes_require_ssl_credentials() {
        let result = validate_ssl_for_deployment(
            &["*.example.com".to_string()],
            tako_core::SslProvider::LetsEncrypt,
            &SecretsStore::default(),
            "production",
        );

        assert!(result.has_errors());
        assert!(
            result.errors[0].contains(
                "Let’s Encrypt wildcard routes require credential ssl.cloudflare. Run `tako credentials set ssl.cloudflare --env production`."
            ),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn cloudflare_ssl_warns_when_credentials_expire_soon() {
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_credential(
                "production",
                SSL_CLOUDFLARE_CREDENTIAL_NAME,
                crate::config::EncryptedSecretValue::new(
                    "encrypted".to_string(),
                    Some(future_expiry(7)),
                ),
            )
            .unwrap();

        let result = validate_ssl_for_deployment(
            &["app.example.com".to_string()],
            tako_core::SslProvider::Cloudflare,
            &secrets,
            "production",
        );

        assert!(!result.has_errors(), "{:?}", result.errors);
        assert!(
            result.warnings.iter().any(|warning| {
                warning.contains("Credential ssl.cloudflare")
                    && warning.contains("expires within 30 days")
            }),
            "{:?}",
            result.warnings
        );
    }
}
