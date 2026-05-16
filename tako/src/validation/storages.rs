use std::collections::HashSet;

use crate::config::{SecretsStore, TakoToml};

use super::ValidationResult;

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
                if secrets
                    .get_storage_credentials(env_name, resource_name)
                    .is_none()
                {
                    result.error(format!(
                        "Storage '{binding_name}' uses S3 resource '{resource_name}' but credentials are missing for environment '{env_name}'"
                    ));
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
                EncryptedStorageCredentials {
                    access_key_id: "encrypted-key".to_string(),
                    secret_access_key: "encrypted-secret".to_string(),
                },
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
}
