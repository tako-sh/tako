use clap::Subcommand;
use std::collections::HashMap;
use std::path::Path;

use crate::config::{EncryptedStorageBinding, StoragesStore, validate_storage_name};
use crate::output;

#[derive(Subcommand)]
pub enum StorageCommands {
    /// Attach an S3-compatible bucket to this app
    Add {
        /// Storage binding name, e.g. uploads
        name: String,
        /// Environment to attach storage for
        #[arg(long, default_value = "production")]
        env: String,
        /// Storage provider
        #[arg(long, default_value = "s3")]
        provider: StorageProviderArg,
        /// Bucket name
        #[arg(long)]
        bucket: String,
        /// S3-compatible endpoint, e.g. https://s3.amazonaws.com or https://<account>.r2.cloudflarestorage.com
        #[arg(long)]
        endpoint: String,
        /// Region. Use auto for R2.
        #[arg(long, default_value = "auto")]
        region: String,
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
    S3,
    R2,
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
            name,
            env,
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
    name: String,
    env: String,
    provider: StorageProviderArg,
    bucket: String,
    endpoint: String,
    region: String,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    force_path_style: bool,
    public_base_url: Option<String>,
}

fn add_storage(input: StorageAddInput<'_>) -> Result<(), Box<dyn std::error::Error>> {
    crate::config::validate_environment_name(&input.env)?;
    validate_storage_name(&input.name)?;
    validate_endpoint(&input.endpoint)?;
    if let Some(public_base_url) = &input.public_base_url {
        validate_endpoint(public_base_url)?;
    }

    let mut secrets = crate::config::SecretsStore::load_from_dir(input.project_dir)?;
    let key_id = secrets.ensure_env_key_id(&input.env)?;
    secrets.save_to_dir(input.project_dir)?;
    let key = crate::commands::secret::load_or_create_key_for_set(
        &input.env,
        &secrets,
        Some(input.project_dir),
    )?;

    let access_key_id = read_storage_credential(input.access_key_id, "Access key id")?;
    let secret_access_key = read_storage_credential(input.secret_access_key, "Secret access key")?;

    let mut storages = StoragesStore::load_from_dir(input.project_dir)?;
    storages.set_env_key_id(&input.env, &key_id)?;
    let binding = EncryptedStorageBinding {
        provider: match input.provider {
            StorageProviderArg::S3 => tako_core::StorageProvider::S3,
            StorageProviderArg::R2 => tako_core::StorageProvider::R2,
        },
        bucket: input.bucket,
        endpoint: trim_trailing_slash(&input.endpoint),
        region: input.region,
        access_key_id: crate::crypto::encrypt(&access_key_id, &key)?,
        secret_access_key: crate::crypto::encrypt(&secret_access_key, &key)?,
        force_path_style: input.force_path_style,
        public_base_url: input
            .public_base_url
            .map(|value| trim_trailing_slash(&value)),
    };
    storages.set(&input.env, &input.name, binding)?;
    storages.save_to_dir(input.project_dir)?;

    output::success(&format!(
        "Attached storage {} to {}.",
        output::strong(&input.name),
        output::strong(&input.env)
    ));
    output::hint("Deploy to sync the storage binding to your server.");
    Ok(())
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
    storages: &StoragesStore,
    usage_path: Option<&Path>,
) -> Result<HashMap<String, tako_core::StorageBinding>, Box<dyn std::error::Error>> {
    let encrypted = match storages.get_env(env) {
        Some(map) if !map.is_empty() => map,
        _ => return Ok(HashMap::new()),
    };
    let key_id = storages
        .get_key_id(env)
        .ok_or_else(|| format!("No storage key configured for environment '{}'.", env))?;
    let key_store = crate::crypto::KeyStore::for_key_id(key_id)?;
    let Some(key) = key_store.load_key_optional_with_usage_path(usage_path)? else {
        return Err(format!(
            "Unable to decrypt {env} storage bindings. Run `tako secrets key import` to import an exported key or passphrase."
        )
        .into());
    };

    let mut decrypted = HashMap::new();
    for (name, binding) in encrypted {
        decrypted.insert(
            name.clone(),
            tako_core::StorageBinding {
                provider: binding.provider,
                bucket: binding.bucket.clone(),
                endpoint: binding.endpoint.clone(),
                region: binding.region.clone(),
                access_key_id: crate::crypto::decrypt(&binding.access_key_id, &key)?,
                secret_access_key: crate::crypto::decrypt(&binding.secret_access_key, &key)?,
                force_path_style: binding.force_path_style,
                public_base_url: binding.public_base_url.clone(),
            },
        );
    }
    Ok(decrypted)
}
