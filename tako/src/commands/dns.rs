use clap::Subcommand;
use std::path::Path;

use crate::config::EncryptedDnsCredentials;
use crate::output;

#[derive(Subcommand)]
pub enum DnsCommands {
    /// Configure wildcard certificate DNS for this app
    Configure {
        /// Environment to configure
        #[arg(long, default_value = "production")]
        env: String,

        /// Cloudflare API token. Prompted when omitted.
        #[arg(long)]
        cloudflare_api_token: Option<String>,

        /// Optional expiry for the Cloudflare API token. Use YYYY-MM-DD, "in 30 days", YYYY-MM-DDTHH:MM:SSZ, or never.
        #[arg(long)]
        expires_at: Option<String>,
    },
}

pub fn run(cmd: DnsCommands, config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let context = crate::commands::project_context::resolve_existing(config_path)?;
    match cmd {
        DnsCommands::Configure {
            env,
            cloudflare_api_token,
            expires_at,
        } => configure_dns(DnsConfigureInput {
            project_dir: &context.project_dir,
            env: &env,
            cloudflare_api_token,
            expires_at,
            print_success: true,
        }),
    }
}

struct DnsConfigureInput<'a> {
    project_dir: &'a Path,
    env: &'a str,
    cloudflare_api_token: Option<String>,
    expires_at: Option<String>,
    print_success: bool,
}

fn configure_dns(input: DnsConfigureInput<'_>) -> Result<(), Box<dyn std::error::Error>> {
    configure_env_dns(
        input.project_dir,
        input.env,
        input.cloudflare_api_token,
        input.expires_at,
        input.print_success,
    )
}

pub(crate) fn configure_env_dns(
    project_dir: &Path,
    env: &str,
    cloudflare_api_token: Option<String>,
    expires_at: Option<String>,
    print_success: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::config::validate_environment_name(env)?;
    let cloudflare_api_token = read_dns_credential(cloudflare_api_token, "Cloudflare API token")?;
    let expires_at = crate::commands::secret::read_secret_expires_at(expires_at, "Expires on")?;

    let mut secrets = crate::config::SecretsStore::load_from_dir(project_dir)?;
    secrets.ensure_env_key_id(env)?;
    let key =
        crate::commands::secret::load_or_create_key_for_set(env, &secrets, Some(project_dir))?;
    secrets.set_dns_credentials(
        env,
        EncryptedDnsCredentials {
            cloudflare_api_token: crate::config::EncryptedSecretValue::new(
                crate::crypto::encrypt(&cloudflare_api_token, &key)?,
                expires_at,
            ),
        },
    )?;
    secrets.save_to_dir(project_dir)?;

    if print_success {
        output::success(&format!(
            "Saved DNS credentials for {}.",
            output::strong(env)
        ));
        output::hint("Deploy to sync DNS credentials to your server.");
    }
    Ok(())
}

pub(crate) fn read_dns_credential(
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

pub(crate) fn decrypt_dns_binding(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<Option<tako_core::DnsBinding>, Box<dyn std::error::Error>> {
    let Some(encrypted) = secrets.get_dns_credentials(env) else {
        return Ok(None);
    };
    let key = crate::commands::secret::load_secret_key(env, secrets, usage_path)?;
    Ok(Some(tako_core::DnsBinding {
        provider: tako_core::DnsProvider::Cloudflare,
        cloudflare_api_token: Some(crate::crypto::decrypt(
            &encrypted.cloudflare_api_token.value,
            &key,
        )?),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn with_temp_tako_home<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::paths::test_tako_home_env_lock();
        let temp = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("TAKO_HOME");
        unsafe {
            std::env::set_var("TAKO_HOME", temp.path());
        }
        let result = f();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("TAKO_HOME", value),
                None => std::env::remove_var("TAKO_HOME"),
            }
        }
        result
    }

    #[test]
    fn configure_env_dns_saves_secret_without_editing_tako_toml() {
        with_temp_tako_home(|| {
            let project = tempfile::TempDir::new().unwrap();
            let config_path = project.path().join("tako.toml");
            let original_toml = r#"name = "app"

[envs.production]
route = "*.example.com"
"#;
            fs::write(&config_path, original_toml).unwrap();

            configure_env_dns(
                project.path(),
                "production",
                Some("cloudflare-token".to_string()),
                Some("never".to_string()),
                false,
            )
            .unwrap();

            assert_eq!(fs::read_to_string(&config_path).unwrap(), original_toml);
            let secrets = crate::config::SecretsStore::load_from_dir(project.path()).unwrap();
            assert!(secrets.get_dns_credentials("production").is_some());
        });
    }
}
