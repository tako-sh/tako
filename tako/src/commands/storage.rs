use clap::Subcommand;
use std::collections::HashMap;
use std::path::Path;

use base64::Engine;

use crate::config::{
    EncryptedStorageCredentials, StorageResourceConfig, TakoToml, validate_storage_name,
};
use crate::output;

#[derive(Subcommand)]
pub enum StorageCommands {
    /// Attach a storage resource to this app
    Add {
        /// App storage binding name, e.g. uploads
        name: String,
        /// Environment to attach storage for
        #[arg(long, default_value = "production")]
        env: String,
        /// Backing storage resource name. Defaults to the binding name.
        #[arg(long)]
        resource: Option<String>,
        /// Storage provider
        #[arg(long, default_value = "s3")]
        provider: StorageProviderArg,
        /// Bucket name
        #[arg(long)]
        bucket: Option<String>,
        /// S3-compatible endpoint, e.g. https://s3.amazonaws.com or https://<account>.r2.cloudflarestorage.com
        #[arg(long)]
        endpoint: Option<String>,
        /// Region. Use auto for R2.
        #[arg(long)]
        region: Option<String>,
        /// Access key id. Prompted when omitted.
        #[arg(long)]
        access_key_id: Option<String>,
        /// Secret access key. Prompted when omitted.
        #[arg(long)]
        secret_access_key: Option<String>,
        /// Use path-style bucket URLs instead of virtual-hosted bucket URLs
        #[arg(long)]
        force_path_style: bool,
        /// Public origin/base URL for public object URLs
        #[arg(long)]
        public_base_url: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum StorageProviderArg {
    Local,
    S3,
}

pub fn run(
    cmd: StorageCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = crate::commands::project_context::resolve_existing(config_path)?;
    match cmd {
        StorageCommands::Add {
            name,
            env,
            resource,
            provider,
            bucket,
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            force_path_style,
            public_base_url,
        } => add_storage(StorageAddInput {
            project_dir: &context.project_dir,
            config_path: &context.config_path,
            name,
            env,
            resource,
            provider,
            bucket,
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            force_path_style,
            public_base_url,
        }),
    }
}

struct StorageAddInput<'a> {
    project_dir: &'a Path,
    config_path: &'a Path,
    name: String,
    env: String,
    resource: Option<String>,
    provider: StorageProviderArg,
    bucket: Option<String>,
    endpoint: Option<String>,
    region: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    force_path_style: bool,
    public_base_url: Option<String>,
}

fn add_storage(input: StorageAddInput<'_>) -> Result<(), Box<dyn std::error::Error>> {
    crate::config::validate_environment_name(&input.env)?;
    validate_storage_name(&input.name)?;
    let resource_name = input.resource.as_deref().unwrap_or(&input.name);
    validate_storage_name(resource_name)?;

    if let Some(public_base_url) = &input.public_base_url {
        validate_endpoint(public_base_url)?;
    }

    let resource = match input.provider {
        StorageProviderArg::Local => StorageResourceConfig {
            provider: tako_core::StorageProvider::Local,
            ..StorageResourceConfig::default()
        },
        StorageProviderArg::S3 => {
            let bucket = required_option(input.bucket, "Bucket")?;
            let endpoint = required_option(input.endpoint, "Endpoint")?;
            validate_endpoint(&endpoint)?;
            StorageResourceConfig {
                provider: tako_core::StorageProvider::S3,
                bucket: Some(bucket),
                endpoint: Some(trim_trailing_slash(&endpoint)),
                region: Some(input.region.unwrap_or_else(|| "auto".to_string())),
                force_path_style: input.force_path_style,
                public_base_url: input
                    .public_base_url
                    .map(|value| trim_trailing_slash(&value)),
            }
        }
    };

    TakoToml::upsert_storage_binding_in_file(
        input.config_path,
        &input.env,
        &input.name,
        resource_name,
        &resource,
    )?;

    if matches!(input.provider, StorageProviderArg::S3) {
        let mut secrets = crate::config::SecretsStore::load_from_dir(input.project_dir)?;
        secrets.ensure_env_key_id(&input.env)?;
        let key = crate::commands::secret::load_or_create_key_for_set(
            &input.env,
            &secrets,
            Some(input.project_dir),
        )?;

        let access_key_id = read_storage_credential(input.access_key_id, "Access key id")?;
        let secret_access_key =
            read_storage_credential(input.secret_access_key, "Secret access key")?;

        secrets.set_storage_credentials(
            &input.env,
            resource_name,
            EncryptedStorageCredentials {
                access_key_id: crate::crypto::encrypt(&access_key_id, &key)?,
                secret_access_key: crate::crypto::encrypt(&secret_access_key, &key)?,
            },
        )?;
        secrets.save_to_dir(input.project_dir)?;
    }

    output::success(&format!(
        "Attached storage {} to {}.",
        output::strong(&input.name),
        output::strong(&input.env)
    ));
    output::hint("Deploy to sync the storage binding to your server.");
    Ok(())
}

fn required_option(
    value: Option<String>,
    label: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("{label} is required for S3 storage.").into()),
    }
}

fn read_storage_credential(
    value: Option<String>,
    prompt: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(value) = value
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    let value = crate::output::password_field(prompt)?;
    if value.trim().is_empty() {
        return Err(format!("{prompt} cannot be empty.").into());
    }
    Ok(value)
}

fn validate_endpoint(endpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = reqwest::Url::parse(endpoint)?;
    if url.scheme() != "https" {
        return Err("Storage endpoints must use https.".into());
    }
    if url.host_str().is_none() {
        return Err("Storage endpoint must include a host.".into());
    }
    Ok(())
}

fn trim_trailing_slash(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

pub(crate) fn decrypt_storage_bindings(
    env: &str,
    config: &TakoToml,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<HashMap<String, tako_core::StorageBinding>, Box<dyn std::error::Error>> {
    let Some(env_config) = config.envs.get(env) else {
        return Ok(HashMap::new());
    };

    let mut decrypted = HashMap::new();
    let mut key_cache: Option<crate::crypto::EncryptionKey> = None;
    for (binding_name, resource_name) in &env_config.storages {
        let Some(resource) = config.storage_resource_for_env(env, resource_name) else {
            return Err(format!(
                "Storage binding '{binding_name}' references missing resource '{resource_name}'."
            )
            .into());
        };
        let binding = match resource.provider {
            tako_core::StorageProvider::Local => tako_core::StorageBinding {
                provider: tako_core::StorageProvider::Local,
                bucket: None,
                endpoint: None,
                region: None,
                access_key_id: None,
                secret_access_key: None,
                force_path_style: false,
                public_base_url: None,
                path: Some(format!("storage/{resource_name}")),
                signing_key: Some(generate_local_storage_signing_key()?),
            },
            tako_core::StorageProvider::S3 => {
                let encrypted = secrets
                    .get_storage_credentials(env, resource_name)
                    .ok_or_else(|| {
                        format!(
                            "Missing storage credentials for resource '{resource_name}' in environment '{env}'."
                        )
                    })?;
                let key = match &key_cache {
                    Some(key) => key.clone(),
                    None => {
                        let loaded =
                            crate::commands::secret::load_secret_key(env, secrets, usage_path)?;
                        key_cache = Some(loaded.clone());
                        loaded
                    }
                };
                tako_core::StorageBinding {
                    provider: tako_core::StorageProvider::S3,
                    bucket: resource.bucket.clone(),
                    endpoint: resource.endpoint.clone(),
                    region: resource.region.clone(),
                    access_key_id: Some(crate::crypto::decrypt(&encrypted.access_key_id, &key)?),
                    secret_access_key: Some(crate::crypto::decrypt(
                        &encrypted.secret_access_key,
                        &key,
                    )?),
                    force_path_style: resource.force_path_style,
                    public_base_url: resource.public_base_url.clone(),
                    path: None,
                    signing_key: None,
                }
            }
        };
        decrypted.insert(binding_name.clone(), binding);
    }
    Ok(decrypted)
}

fn generate_local_storage_signing_key() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
