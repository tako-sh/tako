use crate::config::SecretsStore;

use super::{SECRET_EXPIRY_WARNING_DAYS, ValidationResult};

/// Validate secrets configuration
pub fn validate_secrets(secrets: &SecretsStore) -> ValidationResult {
    let mut result = ValidationResult::new();

    // Check for discrepancies (secrets missing in some environments)
    let discrepancies = secrets.find_discrepancies();
    for discrepancy in &discrepancies {
        result.warn(format!(
            "Secret '{}' is missing in environments: {}",
            discrepancy.name,
            discrepancy.missing_in.join(", ")
        ));
    }

    result
}

/// Validate secrets for a specific environment
///
/// This is stricter than general validation - any missing secrets
/// compared to other environments is an error, not a warning.
pub fn validate_secrets_for_env(secrets: &SecretsStore, env_name: &str) -> ValidationResult {
    let mut result = ValidationResult::new();

    let discrepancies = secrets.find_discrepancies();
    for discrepancy in &discrepancies {
        if discrepancy.missing_in.contains(&env_name.to_string()) {
            result.error(format!(
                "Secret '{}' is missing for environment '{}'. \
                 Run 'tako secret set --env {} {}' to set it.",
                discrepancy.name, env_name, env_name, discrepancy.name
            ));
        }
    }

    result
}

/// Pre-deployment validation of secrets
///
/// Ensures all secrets are complete for the target environment.
/// Returns errors if any secrets are missing.
pub fn validate_secrets_for_deployment(secrets: &SecretsStore, env_name: &str) -> ValidationResult {
    let mut result = ValidationResult::new();

    // First check if environment has any secrets at all
    let env_secrets = secrets.get_env(env_name);
    if env_secrets.is_none() || env_secrets.map(|s| s.is_empty()).unwrap_or(true) {
        // If no environments have app secrets, this is fine. Provider
        // credentials and storage credentials are validated by their own
        // deploy checks.
        if secrets.total_count() == 0 {
            return result;
        }

        // But if other environments have secrets, this environment should too
        result.error(format!(
            "Environment '{}' has no secrets configured, but other environments do. \
             Run 'tako secret sync' to sync secrets.",
            env_name
        ));
        return result;
    }

    // Check for missing secrets compared to other environments
    result.merge(validate_secrets_for_env(secrets, env_name));
    // Warn (do not fail) when the target environment has additional secret names.
    result.merge(validate_target_only_secret_names(secrets, env_name));
    result.merge(validate_target_secret_expirations(secrets, env_name));

    result
}

fn validate_target_only_secret_names(secrets: &SecretsStore, env_name: &str) -> ValidationResult {
    let mut result = ValidationResult::new();
    let Some(target_secrets) = secrets.get_env(env_name) else {
        return result;
    };

    let env_names = secrets.environment_names();
    for secret_name in target_secrets.keys() {
        let mut missing_in = Vec::new();
        for other_env in &env_names {
            if other_env == env_name {
                continue;
            }
            if !secrets.contains(other_env, secret_name) {
                missing_in.push(other_env.clone());
            }
        }

        if !missing_in.is_empty() {
            result.warn(format!(
                "Secret '{}' exists in environment '{}' but is missing in: {}.",
                secret_name,
                env_name,
                missing_in.join(", ")
            ));
        }
    }

    result
}

fn validate_target_secret_expirations(secrets: &SecretsStore, env_name: &str) -> ValidationResult {
    let mut result = ValidationResult::new();
    let Some(target_secrets) = secrets.get_env(env_name) else {
        return result;
    };

    for (secret_name, secret) in target_secrets {
        match secret.is_expired() {
            Ok(true) => {
                if let Some(expires_on) = &secret.expires_on {
                    result.error(format!(
                        "Secret '{secret_name}' in environment '{env_name}' expired on {expires_on}. Run `tako secrets set {secret_name} --env {env_name}` to update it."
                    ));
                }
            }
            Ok(false) => match secret.is_expiring_within_days(SECRET_EXPIRY_WARNING_DAYS) {
                Ok(true) => {
                    if let Some(expires_on) = &secret.expires_on {
                        result.warn(format!(
                            "Secret '{secret_name}' in environment '{env_name}' expires within {SECRET_EXPIRY_WARNING_DAYS} days on {expires_on}. Run `tako secrets set {secret_name} --env {env_name}` to rotate it."
                        ));
                    }
                }
                Ok(false) => {}
                Err(error) => result.error(format!(
                    "Secret '{secret_name}' in environment '{env_name}' has invalid expiry metadata: {error}"
                )),
            },
            Err(error) => result.error(format!(
                "Secret '{secret_name}' in environment '{env_name}' has invalid expiry metadata: {error}"
            )),
        }
    }

    result
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
    fn deploy_validation_warns_when_target_env_has_extra_secret_names() {
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets.ensure_env_key_id("staging").unwrap();
        secrets
            .set("production", "API_KEY", "x".to_string())
            .unwrap();
        secrets
            .set("production", "ONLY_PROD", "y".to_string())
            .unwrap();
        secrets.set("staging", "API_KEY", "z".to_string()).unwrap();

        let result = validate_secrets_for_deployment(&secrets, "production");
        assert!(
            !result.has_errors(),
            "unexpected errors: {:?}",
            result.errors
        );
        assert!(result.has_warnings());
        assert!(result.warnings.iter().any(|w| {
            w.contains("ONLY_PROD") && w.contains("missing in: staging") && w.contains("production")
        }));
    }

    #[test]
    fn deploy_validation_allows_credentials_without_app_secrets() {
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_credential(
                "production",
                crate::config::POSTGRES_CREDENTIAL_NAME,
                crate::config::EncryptedSecretValue::new("encrypted".to_string(), None),
            )
            .unwrap();

        let result = validate_secrets_for_deployment(&secrets, "production");

        assert!(
            !result.has_errors(),
            "unexpected errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn deploy_validation_passes_when_secret_names_match_between_envs() {
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets.ensure_env_key_id("staging").unwrap();
        secrets
            .set("production", "API_KEY", "x".to_string())
            .unwrap();
        secrets
            .set("production", "DB_URL", "y".to_string())
            .unwrap();
        secrets.set("staging", "API_KEY", "z".to_string()).unwrap();
        secrets.set("staging", "DB_URL", "w".to_string()).unwrap();

        let result = validate_secrets_for_deployment(&secrets, "production");
        assert!(
            !result.has_errors(),
            "unexpected errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn deploy_validation_fails_when_target_secret_is_expired() {
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_with_expires_on(
                "production",
                "API_KEY",
                "encrypted".to_string(),
                Some("2000-01-01".to_string()),
            )
            .unwrap();

        let result = validate_secrets_for_deployment(&secrets, "production");

        assert!(result.has_errors());
        assert!(
            result.errors.iter().any(|error| {
                error.contains("API_KEY") && error.contains("expired on 2000-01-01")
            }),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn deploy_validation_warns_when_target_secret_expires_soon() {
        let mut secrets = SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_with_expires_on(
                "production",
                "API_KEY",
                "encrypted".to_string(),
                Some(future_expiry(7)),
            )
            .unwrap();

        let result = validate_secrets_for_deployment(&secrets, "production");

        assert!(!result.has_errors(), "{:?}", result.errors);
        assert!(
            result.warnings.iter().any(|warning| {
                warning.contains("API_KEY") && warning.contains("expires within 30 days")
            }),
            "{:?}",
            result.warnings
        );
    }
}
