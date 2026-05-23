use clap::Subcommand;
use std::path::Path;

use crate::config::{EncryptedSecretValue, SSL_CLOUDFLARE_CREDENTIAL_NAME};
use crate::output;

#[derive(Subcommand)]
pub enum CredentialCommands {
    /// Set a provider credential used by Tako
    Set {
        /// Credential name
        name: String,

        /// Environment to set the credential for
        #[arg(long)]
        env: Option<String>,

        /// Optional expiry date. Use YYYY-MM-DD, "in 30 days", or never.
        #[arg(long)]
        expires_on: Option<String>,
    },

    /// Remove a provider credential
    #[command(visible_aliases = ["remove", "delete", "del"])]
    Rm {
        /// Credential name
        name: String,

        /// Environment to remove from
        #[arg(long)]
        env: Option<String>,
    },

    /// List provider credentials
    #[command(visible_aliases = ["ls", "show"])]
    List,
}

pub fn run(
    cmd: CredentialCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = crate::commands::project_context::resolve_existing(config_path)?;
    match cmd {
        CredentialCommands::Set {
            name,
            env,
            expires_on,
        } => set_credential_command(&context, &name, env.as_deref(), expires_on),
        CredentialCommands::Rm { name, env } => {
            remove_credential_command(&context, &name, env.as_deref())
        }
        CredentialCommands::List => list_credentials(&context),
    }
}

fn set_credential_command(
    context: &crate::commands::project_context::ProjectContext,
    name: &str,
    requested_env: Option<&str>,
    requested_expires_on: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::config::validate_credential_name(name)?;
    let env = crate::commands::secret::resolve_secret_environment(
        context,
        requested_env,
        "Credential environment",
    )?;
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    if !confirm_credential_override(&secrets, name, &env)? {
        return Ok(());
    }
    let value = read_credential_value(None, credential_value_prompt(name, &secrets, &env))?;
    let expires_on =
        crate::commands::secret::read_secret_expires_on(requested_expires_on, "Expires on")?;
    let existed = secrets.contains_credential(&env, name);
    set_credential_value(&context.project_dir, &env, name, &value, expires_on)?;

    if existed {
        output::success(&format!(
            "Updated credential {} in {}",
            output::strong(name),
            output::strong(&env)
        ));
    } else {
        output::success(&format!(
            "Set credential {} in {}",
            output::strong(name),
            output::strong(&env)
        ));
    }

    Ok(())
}

fn remove_credential_command(
    context: &crate::commands::project_context::ProjectContext,
    name: &str,
    requested_env: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::config::validate_credential_name(name)?;
    let env = crate::commands::secret::resolve_secret_environment(
        context,
        requested_env,
        "Credential environment",
    )?;
    let mut secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    if !secrets.contains_credential(&env, name) {
        return Err(format!("Credential '{name}' not found in environment '{env}'").into());
    }

    let confirm = output::confirm(
        &format!(
            "Remove credential {} from {}?",
            output::strong(name),
            output::strong(&env)
        ),
        false,
    )?;
    if !confirm {
        output::operation_cancelled();
        return Ok(());
    }

    secrets.remove_credential(&env, name)?;
    secrets.save_to_dir(&context.project_dir)?;
    output::success(&format!(
        "Removed credential {} from {}",
        output::strong(name),
        output::strong(&env)
    ));
    Ok(())
}

fn list_credentials(
    context: &crate::commands::project_context::ProjectContext,
) -> Result<(), Box<dyn std::error::Error>> {
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let all_names = secrets.all_credential_names();
    if all_names.is_empty() {
        output::warning("No credentials configured.");
        output::muted(&format!(
            "Run {} to add one.",
            output::strong("tako credentials set ssl.cloudflare")
        ));
        return Ok(());
    }

    output::section("Credentials");
    let all_envs = secrets.environment_names();
    if output::is_pretty() {
        eprint!("{:<30}", "CREDENTIAL");
        for env in &all_envs {
            eprint!(" {:<15}", env.to_uppercase());
        }
        eprintln!();

        eprint!("{}", "-".repeat(30));
        for _ in &all_envs {
            eprint!(" {}", "-".repeat(15));
        }
        eprintln!();

        for name in &all_names {
            eprint!("{:<30}", name);
            for env in &all_envs {
                if secrets.contains_credential(env, name) {
                    eprint!(" {:<15}", "[set]");
                } else {
                    eprint!(" {:<15}", "-");
                }
            }
            eprintln!();
        }
    } else {
        for name in &all_names {
            let envs_with_credential: Vec<&str> = all_envs
                .iter()
                .filter(|env| secrets.contains_credential(env, name))
                .map(|s| s.as_str())
                .collect();
            tracing::info!("{name}: set in {}", envs_with_credential.join(", "));
        }
    }

    Ok(())
}

fn confirm_credential_override(
    secrets: &crate::config::SecretsStore,
    name: &str,
    env: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    if !secrets.contains_credential(env, name) || !output::is_interactive() {
        return Ok(true);
    }

    let confirmed = output::confirm("Credential is already set. Replace it?", false)?;
    if !confirmed {
        output::operation_cancelled();
    }
    Ok(confirmed)
}

fn credential_value_prompt(
    name: &str,
    secrets: &crate::config::SecretsStore,
    env: &str,
) -> &'static str {
    match name {
        SSL_CLOUDFLARE_CREDENTIAL_NAME if secrets.contains_credential(env, name) => {
            "Enter new Cloudflare API token"
        }
        SSL_CLOUDFLARE_CREDENTIAL_NAME => "Cloudflare API token",
        _ => "Credential value",
    }
}

pub(crate) fn read_credential_value(
    value: Option<String>,
    prompt: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::IsTerminal;

    if let Some(value) = value
        && !value.trim().is_empty()
    {
        return Ok(value);
    }

    if std::io::stdin().is_terminal() {
        let value = output::password_field(prompt)?;
        if value.trim().is_empty() {
            return Err(format!("{prompt} cannot be empty.").into());
        }
        return Ok(value);
    }

    let mut value = String::new();
    let bytes = std::io::stdin().read_line(&mut value)?;
    if bytes == 0 {
        return Err("No credential value provided on stdin".into());
    }
    let value = value.trim_end_matches(['\r', '\n']).to_string();
    if value.trim().is_empty() {
        return Err("Credential value cannot be empty".into());
    }
    Ok(value)
}

pub(crate) fn set_ssl_cloudflare_credential(
    project_dir: &Path,
    env: &str,
    value: &str,
    expires_on: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    set_credential_value(
        project_dir,
        env,
        SSL_CLOUDFLARE_CREDENTIAL_NAME,
        value,
        expires_on,
    )
}

fn set_credential_value(
    project_dir: &Path,
    env: &str,
    name: &str,
    value: &str,
    expires_on: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::config::validate_credential_name(name)?;
    let mut secrets = crate::config::SecretsStore::load_from_dir(project_dir)?;
    secrets.ensure_env_key_id(env)?;
    let key =
        crate::commands::secret::load_or_create_key_for_set(env, &secrets, Some(project_dir))?;
    let encrypted = crate::crypto::encrypt(value, &key)?;
    secrets.set_credential(env, name, EncryptedSecretValue::new(encrypted, expires_on))?;
    secrets.save_to_dir(project_dir)?;
    Ok(())
}

pub(crate) fn decrypt_ssl_binding(
    env: &str,
    provider: tako_core::SslProvider,
    routes: &[String],
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<tako_core::SslBinding, Box<dyn std::error::Error>> {
    let needs_token = provider == tako_core::SslProvider::Cloudflare
        || (provider == tako_core::SslProvider::LetsEncrypt
            && crate::validation::letsencrypt_routes_need_cloudflare_token(routes));
    if !needs_token {
        return Ok(tako_core::SslBinding::default());
    }

    let Some(encrypted) = secrets.get_credential(env, SSL_CLOUDFLARE_CREDENTIAL_NAME) else {
        return Err(missing_ssl_cloudflare_credential_message(env, provider).into());
    };
    let key = crate::commands::secret::load_secret_key(env, secrets, usage_path)?;
    Ok(tako_core::SslBinding {
        provider,
        cloudflare_api_token: Some(crate::crypto::decrypt(&encrypted.value, &key)?),
    })
}

pub(crate) fn missing_ssl_cloudflare_credential_message(
    env: &str,
    provider: tako_core::SslProvider,
) -> String {
    match provider {
        tako_core::SslProvider::Cloudflare => {
            format!(
                "Cloudflare SSL requires credential {SSL_CLOUDFLARE_CREDENTIAL_NAME}. Run `tako credentials set {SSL_CLOUDFLARE_CREDENTIAL_NAME} --env {env}`."
            )
        }
        tako_core::SslProvider::LetsEncrypt => {
            format!(
                "Let’s Encrypt wildcard routes require credential {SSL_CLOUDFLARE_CREDENTIAL_NAME}. Run `tako credentials set {SSL_CLOUDFLARE_CREDENTIAL_NAME} --env {env}`."
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn set_ssl_cloudflare_credential_saves_generic_credential() {
        with_temp_tako_home(|| {
            let project = tempfile::TempDir::new().unwrap();

            set_ssl_cloudflare_credential(
                project.path(),
                "production",
                "cloudflare-token",
                Some("2099-01-01".to_string()),
            )
            .unwrap();

            let secrets = crate::config::SecretsStore::load_from_dir(project.path()).unwrap();
            let credential = secrets
                .get_credential("production", SSL_CLOUDFLARE_CREDENTIAL_NAME)
                .unwrap();
            assert_eq!(credential.expires_on.as_deref(), Some("2099-01-01"));
        });
    }

    #[test]
    fn decrypt_ssl_binding_reports_credentials_command_when_missing() {
        let err = decrypt_ssl_binding(
            "production",
            tako_core::SslProvider::Cloudflare,
            &["app.example.com".to_string()],
            &crate::config::SecretsStore::default(),
            None,
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "Cloudflare SSL requires credential ssl.cloudflare. Run `tako credentials set ssl.cloudflare --env production`."
        );
    }
}
