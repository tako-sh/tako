use crate::config::SecretsStore;

use super::ValidationResult;

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

    if secrets.get_dns_credentials(env_name).is_none() {
        result.error(format!(
            "Wildcard routes require DNS credentials: {}. Run `tako dns configure --env {env_name}`.",
            wildcard_routes
                .iter()
                .map(|route| route.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
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
                    cloudflare_api_token: "encrypted".to_string(),
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
}
