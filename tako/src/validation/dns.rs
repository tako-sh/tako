use crate::config::SecretsStore;

use super::{SECRET_EXPIRY_WARNING_DAYS, ValidationResult};

pub fn validate_dns_for_deployment(
    routes: &[String],
    secrets: &SecretsStore,
    env_name: &str,
) -> ValidationResult {
    let mut result = ValidationResult::new();
    let wildcard_routes = routes
        .iter()
        .filter(|route| route.starts_with("*."))
        .collect::<Vec<_>>();
    if wildcard_routes.is_empty() {
        return result;
    }

    let Some(credentials) = secrets.get_dns_credentials(env_name) else {
        result.error(format!(
            "Wildcard routes require DNS credentials: {}. Run `tako dns configure --env {env_name}`.",
            wildcard_routes
                .iter()
                .map(|route| route.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return result;
    };

    match credentials.cloudflare_api_token.is_expired() {
        Ok(true) => {
            if let Some(expires_at) = &credentials.cloudflare_api_token.expires_at {
                result.error(format!(
                    "DNS credentials for environment '{env_name}' expired at {expires_at}. Run `tako dns configure --env {env_name}` to update them."
                ));
            }
        }
        Ok(false) => match credentials
            .cloudflare_api_token
            .is_expiring_within_days(SECRET_EXPIRY_WARNING_DAYS)
        {
            Ok(true) => {
                if let Some(expires_at) = &credentials.cloudflare_api_token.expires_at {
                    result.warn(format!(
                        "DNS credentials for environment '{env_name}' expire within {SECRET_EXPIRY_WARNING_DAYS} days at {expires_at}. Run `tako dns configure --env {env_name}` to rotate them."
                    ));
                }
            }
            Ok(false) => {}
            Err(error) => result.error(format!(
                "DNS credentials for environment '{env_name}' have invalid expiry metadata: {error}"
            )),
        },
        Err(error) => result.error(format!(
            "DNS credentials for environment '{env_name}' have invalid expiry metadata: {error}"
        )),
    }

    result
}

pub(crate) fn routes_need_dns(routes: &[String]) -> bool {
    routes.iter().any(|route| route.starts_with("*."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EncryptedDnsCredentials;
    use time::{Duration, OffsetDateTime};

    fn future_expiry(days: i64) -> String {
        let expires_at = OffsetDateTime::now_utc() + Duration::days(days);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            expires_at.year(),
            u8::from(expires_at.month()),
            expires_at.day(),
            expires_at.hour(),
            expires_at.minute(),
            expires_at.second()
        )
    }

    #[test]
    fn wildcard_routes_require_dns_credentials() {
        let routes = vec!["*.example.com".to_string()];

        let result = validate_dns_for_deployment(&routes, &SecretsStore::default(), "production");

        assert!(result.has_errors());
        assert!(
            result.errors[0].contains("Wildcard routes require DNS credentials: *.example.com"),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn wildcard_routes_pass_with_credentials() {
        let routes = vec!["*.example.com".to_string()];
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_dns_credentials(
                "production",
                EncryptedDnsCredentials {
                    cloudflare_api_token: crate::config::EncryptedSecretValue::new(
                        "encrypted".to_string(),
                        Some("2099-01-01T00:00:00Z".to_string()),
                    ),
                },
            )
            .unwrap();

        let result = validate_dns_for_deployment(&routes, &secrets, "production");

        assert!(!result.has_errors(), "{:?}", result.errors);
    }

    #[test]
    fn exact_routes_do_not_require_dns_credentials() {
        let routes = vec!["app.example.com".to_string()];

        let result = validate_dns_for_deployment(&routes, &SecretsStore::default(), "production");

        assert!(!result.has_errors(), "{:?}", result.errors);
    }

    #[test]
    fn wildcard_routes_fail_when_dns_credentials_are_expired() {
        let routes = vec!["*.example.com".to_string()];
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_dns_credentials(
                "production",
                EncryptedDnsCredentials {
                    cloudflare_api_token: crate::config::EncryptedSecretValue::new(
                        "encrypted".to_string(),
                        Some("2000-01-01T00:00:00Z".to_string()),
                    ),
                },
            )
            .unwrap();

        let result = validate_dns_for_deployment(&routes, &secrets, "production");

        assert!(result.has_errors());
        assert!(
            result.errors.iter().any(|error| {
                error.contains("DNS credentials")
                    && error.contains("expired at 2000-01-01T00:00:00Z")
            }),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn wildcard_routes_warn_when_dns_credentials_expire_soon() {
        let routes = vec!["*.example.com".to_string()];
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_dns_credentials(
                "production",
                EncryptedDnsCredentials {
                    cloudflare_api_token: crate::config::EncryptedSecretValue::new(
                        "encrypted".to_string(),
                        Some(future_expiry(7)),
                    ),
                },
            )
            .unwrap();

        let result = validate_dns_for_deployment(&routes, &secrets, "production");

        assert!(!result.has_errors(), "{:?}", result.errors);
        assert!(
            result.warnings.iter().any(|warning| {
                warning.contains("DNS credentials") && warning.contains("expire within 30 days")
            }),
            "{:?}",
            result.warnings
        );
    }
}
