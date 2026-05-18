use std::collections::HashSet;

use crate::config::{SecretsStore, TakoToml};

use super::{SECRET_EXPIRY_WARNING_DAYS, ValidationResult};

pub fn validate_storages_for_deployment(
    config: &TakoToml,
    secrets: &SecretsStore,
    env_name: &str,
    server_count: usize,
) -> ValidationResult {
    let mut result = ValidationResult::new();
    let Some(env) = config.envs.get(env_name) else {
        result.error(format!(
            "Environment '{env_name}' is not declared in tako.toml"
        ));
        return result;
    };

    let mut assigned_resources = HashSet::new();
    for (binding_name, resource_name) in &env.storages {
        assigned_resources.insert(resource_name.as_str());
        let Some(resource) = config.storage_resource_for_env(env_name, resource_name) else {
            result.error(format!(
                "Environment '{env_name}' storage '{binding_name}' references missing storage resource '{resource_name}'"
            ));
            continue;
        };

        match resource.provider {
            tako_core::StorageProvider::Local => {
                if env_name != "development" && server_count != 1 {
                    result.error(format!(
                        "Storage '{binding_name}' uses local storage in environment '{env_name}', but local storage requires exactly one server"
                    ));
                }
            }
            tako_core::StorageProvider::S3 => {
                let mut warned_expiring_at = HashSet::new();
                let Some(credentials) = secrets.get_storage_credentials(env_name, resource_name)
                else {
                    result.error(format!(
                        "Storage '{binding_name}' uses S3 resource '{resource_name}' but credentials are missing for environment '{env_name}'"
                    ));
                    continue;
                };
                for (field, secret) in [
                    ("access key id", &credentials.access_key_id),
                    ("secret access key", &credentials.secret_access_key),
                ] {
                    match secret.is_expired() {
                        Ok(true) => {
                            if let Some(expires_at) = &secret.expires_at {
                                result.error(format!(
                                    "Storage credentials for '{resource_name}' in environment '{env_name}' expired at {expires_at} ({field}). Run `tako storages add {binding_name} --env {env_name}` to update them."
                                ));
                            }
                        }
                        Ok(false) => {
                            match secret.is_expiring_within_days(SECRET_EXPIRY_WARNING_DAYS) {
                                Ok(true) => {
                                    if let Some(expires_at) = &secret.expires_at
                                        && warned_expiring_at.insert(expires_at.clone())
                                    {
                                        result.warn(format!(
                                            "Storage credentials for '{resource_name}' in environment '{env_name}' expire within {SECRET_EXPIRY_WARNING_DAYS} days at {expires_at}. Run `tako storages add {binding_name} --env {env_name}` to rotate them."
                                        ));
                                    }
                                }
                                Ok(false) => {}
                                Err(error) => result.error(format!(
                                    "Storage credentials for '{resource_name}' in environment '{env_name}' have invalid expiry metadata: {error}"
                                )),
                            }
                        }
                        Err(error) => result.error(format!(
                            "Storage credentials for '{resource_name}' in environment '{env_name}' have invalid expiry metadata: {error}"
                        )),
                    }
                }
            }
        }
    }

    if let Some(storage_credentials) = secrets.get_storage_credentials_env(env_name) {
        for resource_name in storage_credentials.keys() {
            if !assigned_resources.contains(resource_name.as_str()) {
                result.error(format!(
                    "Storage credentials for '{resource_name}' exist in environment '{env_name}', but no storage binding uses that resource"
                ));
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EncryptedStorageCredentials;
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

    fn config_with_production_storage() -> TakoToml {
        TakoToml::parse(
            r#"
name = "demo"

[storages.prod_uploads]
provider = "s3"
bucket = "demo-prod-uploads"
endpoint = "https://s3.example.com"
region = "us-east-1"

[envs.production]
route = "demo.example.com"
storages = { uploads = "prod_uploads" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn deploy_validation_fails_when_s3_credentials_are_missing() {
        let config = config_with_production_storage();
        let secrets = SecretsStore::default();
        let result = validate_storages_for_deployment(&config, &secrets, "production", 1);
        assert!(result.has_errors());
        assert!(result.errors[0].contains("credentials are missing"));
    }

    #[test]
    fn deploy_validation_fails_for_unbound_storage_credentials() {
        let config = config_with_production_storage();
        let mut secrets = SecretsStore::default();
        secrets
            .set_env_key_id("production", "0123456789abcdef")
            .unwrap();
        secrets
            .set_storage_credentials(
                "production",
                "old_uploads",
                EncryptedStorageCredentials::new(
                    "encrypted-key".to_string(),
                    "encrypted-secret".to_string(),
                    Some("2099-01-01T00:00:00Z".to_string()),
                ),
            )
            .unwrap();

        let result = validate_storages_for_deployment(&config, &secrets, "production", 1);
        assert!(result.has_errors());
        assert!(result.errors.iter().any(|error| {
            error.contains("old_uploads") && error.contains("no storage binding uses")
        }));
    }

    #[test]
    fn deploy_validation_allows_implicit_development_local_storage() {
        let config = TakoToml::parse(
            r#"
name = "demo"

[envs.development]
storages = { uploads = "uploads" }
"#,
        )
        .unwrap();
        let result =
            validate_storages_for_deployment(&config, &SecretsStore::default(), "development", 0);
        assert!(!result.has_errors(), "{:?}", result.errors);
    }

    #[test]
    fn deploy_validation_allows_implicit_production_local_storage() {
        let config = TakoToml::parse(
            r#"
name = "demo"

[envs.production]
route = "demo.example.com"
servers = ["prod-a"]
storages = { uploads = "local" }
"#,
        )
        .unwrap();
        let result =
            validate_storages_for_deployment(&config, &SecretsStore::default(), "production", 1);
        assert!(!result.has_errors(), "{:?}", result.errors);
    }

    #[test]
    fn deploy_validation_fails_when_s3_credentials_are_expired() {
        let config = config_with_production_storage();
        let mut secrets = SecretsStore::default();
        secrets
            .set_env_key_id("production", "0123456789abcdef")
            .unwrap();
        secrets
            .set_storage_credentials(
                "production",
                "prod_uploads",
                EncryptedStorageCredentials::new(
                    "encrypted-key".to_string(),
                    "encrypted-secret".to_string(),
                    Some("2000-01-01T00:00:00Z".to_string()),
                ),
            )
            .unwrap();

        let result = validate_storages_for_deployment(&config, &secrets, "production", 1);

        assert!(result.has_errors());
        assert!(
            result.errors.iter().any(|error| {
                error.contains("prod_uploads") && error.contains("expired at 2000-01-01T00:00:00Z")
            }),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn deploy_validation_warns_when_s3_credentials_expire_soon() {
        let config = config_with_production_storage();
        let mut secrets = SecretsStore::default();
        secrets
            .set_env_key_id("production", "0123456789abcdef")
            .unwrap();
        secrets
            .set_storage_credentials(
                "production",
                "prod_uploads",
                EncryptedStorageCredentials::new(
                    "encrypted-key".to_string(),
                    "encrypted-secret".to_string(),
                    Some(future_expiry(7)),
                ),
            )
            .unwrap();

        let result = validate_storages_for_deployment(&config, &secrets, "production", 1);

        assert!(!result.has_errors(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings.iter().any(|warning| {
                warning.contains("prod_uploads") && warning.contains("expire within 30 days")
            }),
            "{:?}",
            result.warnings
        );
    }
}
