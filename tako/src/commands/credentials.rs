use clap::Subcommand;
use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{
    EncryptedSecretValue, POSTGRES_CREDENTIAL_NAME, SSL_CLOUDFLARE_CREDENTIAL_NAME,
};
use crate::output;

#[derive(Subcommand)]
pub enum CredentialCommands {
    /// Set a provider credential used by Tako
    Set {
        /// Credential name
        name: Option<String>,

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
    cmd: Option<CredentialCommands>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = crate::commands::project_context::resolve_existing(config_path)?;
    match cmd {
        Some(CredentialCommands::Set {
            name,
            env,
            expires_on,
        }) => set_credential_command(&context, name.as_deref(), env.as_deref(), expires_on),
        Some(CredentialCommands::Rm { name, env }) => {
            remove_credential_command(&context, &name, env.as_deref())
        }
        Some(CredentialCommands::List) | None => list_credentials(&context),
    }
}

fn set_credential_command(
    context: &crate::commands::project_context::ProjectContext,
    requested_name: Option<&str>,
    requested_env: Option<&str>,
    requested_expires_on: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = resolve_credential_name(requested_name)?;
    let env = resolve_credential_environment(context, requested_env)?;
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    if !confirm_credential_override(&secrets, &name, &env)? {
        return Ok(());
    }
    let value = read_credential_value(None, credential_value_prompt(&name, &secrets, &env))?;
    let expires_on =
        crate::commands::secret::read_secret_expires_on(requested_expires_on, "Expires on")?;
    let existed = secrets.contains_credential(&env, &name);
    set_credential_value(&context.project_dir, &env, &name, &value, expires_on)?;

    if existed {
        output::success(&format!(
            "Updated credential {} in {}",
            output::strong(&name),
            output::strong(&env)
        ));
    } else {
        output::success(&format!(
            "Set credential {} in {}",
            output::strong(&name),
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
    let name = normalize_credential_name(name);
    crate::config::validate_credential_name(&name)?;
    let env = resolve_credential_environment(context, requested_env)?;
    let mut secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    if !secrets.contains_credential(&env, &name) {
        return Err(format!("Credential '{name}' not found in environment '{env}'").into());
    }

    let confirm = output::confirm(
        &format!(
            "Remove credential {} from {}?",
            output::strong(&name),
            output::strong(&env)
        ),
        false,
    )?;
    if !confirm {
        output::operation_cancelled();
        return Ok(());
    }

    secrets.remove_credential(&env, &name)?;
    secrets.save_to_dir(&context.project_dir)?;
    output::success(&format!(
        "Removed credential {} from {}",
        output::strong(&name),
        output::strong(&env)
    ));
    Ok(())
}

fn normalize_credential_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn credential_options() -> Vec<(String, String, &'static str)> {
    vec![
        (
            SSL_CLOUDFLARE_CREDENTIAL_NAME.to_string(),
            SSL_CLOUDFLARE_CREDENTIAL_NAME.to_string(),
            "Cloudflare certificates",
        ),
        (
            POSTGRES_CREDENTIAL_NAME.to_string(),
            POSTGRES_CREDENTIAL_NAME.to_string(),
            "Shared channel and workflow storage",
        ),
    ]
}

fn resolve_credential_name(requested: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(name) = requested {
        let name = normalize_credential_name(name);
        crate::config::validate_credential_name(&name)?;
        return Ok(name);
    }

    if !output::is_interactive() {
        return Err(
            "Missing required credential name. Run `tako credentials set <name>` or run interactively to choose one."
                .into(),
        );
    }

    let options = credential_options();
    let hints: Vec<&str> = options.iter().map(|(_, _, hint)| *hint).collect();
    let choices = options
        .iter()
        .map(|(label, name, _)| (label.clone(), name.clone()))
        .collect();
    let mut wizard = output::Wizard::new().with_fields(&[("Credential", false)]);
    wizard
        .select("Credential", "Credential", choices, &hints, 0)
        .map_err(Into::into)
}

fn credential_environment_options(
    tako_config: &crate::config::TakoToml,
    secrets: &crate::config::SecretsStore,
) -> Vec<(String, String)> {
    let mut names = BTreeSet::new();
    names.extend(tako_config.get_environment_names());
    names.extend(secrets.environment_names());
    names.remove("development");

    let mut options = Vec::new();
    names.remove("production");
    options.push(("production".to_string(), "production".to_string()));
    options.extend(names.into_iter().map(|name| (name.clone(), name)));
    options
}

fn validate_requested_credential_environment(
    env: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    crate::config::validate_environment_name(env)?;
    if env == "development" {
        return Err(
            "Provider credentials are for deployed environments. Use a deployment environment like production."
                .into(),
        );
    }
    Ok(env.to_string())
}

fn resolve_credential_environment(
    context: &crate::commands::project_context::ProjectContext,
    requested: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(env) = requested {
        return validate_requested_credential_environment(env);
    }

    if !output::is_interactive() {
        return Err(
            "Missing required environment. Pass --env with a deployment environment or run interactively to choose one."
                .into(),
        );
    }

    let tako_config = crate::config::TakoToml::load_from_file(&context.config_path)?;
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let mut wizard = output::Wizard::new().with_fields(&[("Environment", false)]);
    wizard
        .select(
            "Environment",
            "Credential environment",
            credential_environment_options(&tako_config, &secrets),
            &[],
            0,
        )
        .map_err(Into::into)
}

#[derive(Debug, PartialEq, Eq)]
struct CredentialStatusRow {
    name: String,
    envs: Vec<(String, bool)>,
}

fn credential_status_rows(
    tako_config: &crate::config::TakoToml,
    secrets: &crate::config::SecretsStore,
) -> (Vec<String>, Vec<CredentialStatusRow>) {
    let mut envs = BTreeSet::new();
    envs.extend(tako_config.get_environment_names());
    envs.extend(secrets.environment_names());
    envs.remove("development");
    let envs: Vec<String> = envs.into_iter().collect();

    let mut names = BTreeSet::new();
    names.extend(
        credential_options()
            .into_iter()
            .map(|(_, credential, _)| credential),
    );
    names.extend(secrets.all_credential_names());

    let rows = names
        .into_iter()
        .map(|name| CredentialStatusRow {
            envs: envs
                .iter()
                .map(|env| (env.clone(), secrets.contains_credential(env, &name)))
                .collect(),
            name,
        })
        .collect();

    (envs, rows)
}

fn list_credentials(
    context: &crate::commands::project_context::ProjectContext,
) -> Result<(), Box<dyn std::error::Error>> {
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let tako_config = crate::config::TakoToml::load_from_file(&context.config_path)?;
    let (all_envs, rows) = credential_status_rows(&tako_config, &secrets);

    output::section("Credentials");
    if output::is_pretty() {
        if all_envs.is_empty() {
            eprintln!("{:<30} {:<15}", "CREDENTIAL", "STATUS");
            eprintln!("{} {}", "-".repeat(30), "-".repeat(15));
        } else {
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
        }

        for row in &rows {
            eprint!("{:<30}", row.name);
            if row.envs.is_empty() {
                eprint!(" {:<15}", "-");
            } else {
                for (_, is_set) in &row.envs {
                    eprint!(" {:<15}", if *is_set { "[set]" } else { "-" });
                }
            }
            eprintln!();
        }
    } else {
        for row in &rows {
            if row.envs.is_empty() {
                tracing::info!("{}: not set", row.name);
                continue;
            }

            let statuses = row
                .envs
                .iter()
                .map(|(env, is_set)| format!("{env}={}", if *is_set { "set" } else { "-" }))
                .collect::<Vec<_>>()
                .join(", ");
            tracing::info!("{}: {}", row.name, statuses);
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
    let name = normalize_credential_name(name);
    crate::config::validate_credential_name(&name)?;
    let mut secrets = crate::config::SecretsStore::load_from_dir(project_dir)?;
    secrets.ensure_env_key_id(env)?;
    let key =
        crate::commands::secret::load_or_create_key_for_set(env, &secrets, Some(project_dir))?;
    let encrypted = crate::crypto::encrypt(value, &key)?;
    secrets.set_credential(env, &name, EncryptedSecretValue::new(encrypted, expires_on))?;
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
    let token = crate::crypto::decrypt(&encrypted.value, &key)?;
    validate_cloudflare_token_for_ssl_binding(provider, routes, &token)?;
    Ok(tako_core::SslBinding {
        provider,
        cloudflare_api_token: Some(token),
    })
}

pub(crate) fn decrypt_runtime_credentials(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<std::collections::HashMap<String, String>, Box<dyn std::error::Error>> {
    let Some(encrypted) = secrets.get_credential(env, POSTGRES_CREDENTIAL_NAME) else {
        return Ok(std::collections::HashMap::new());
    };

    let key = crate::commands::secret::load_secret_key(env, secrets, usage_path)?;
    let value = crate::crypto::decrypt(&encrypted.value, &key)
        .map_err(|e| format!("Failed to decrypt credential '{POSTGRES_CREDENTIAL_NAME}': {e}"))?;
    let mut decrypted = std::collections::HashMap::with_capacity(1);
    decrypted.insert(POSTGRES_CREDENTIAL_NAME.to_string(), value);
    Ok(decrypted)
}

fn validate_cloudflare_token_for_ssl_binding(
    _provider: tako_core::SslProvider,
    _routes: &[String],
    token: &str,
) -> Result<(), String> {
    if token.trim().is_empty() {
        return Err(format!(
            "Credential {SSL_CLOUDFLARE_CREDENTIAL_NAME} cannot be empty."
        ));
    }
    Ok(())
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
    fn set_credential_value_normalizes_input_name_to_lowercase() {
        with_temp_tako_home(|| {
            let project = tempfile::TempDir::new().unwrap();

            set_credential_value(
                project.path(),
                "production",
                "POSTGRES_URL",
                "postgres://runtime",
                None,
            )
            .unwrap();

            let secrets = crate::config::SecretsStore::load_from_dir(project.path()).unwrap();
            assert!(
                secrets
                    .get_credential("production", POSTGRES_CREDENTIAL_NAME)
                    .is_some()
            );
            assert!(
                secrets
                    .get_credential("production", "POSTGRES_URL")
                    .is_none()
            );
        });
    }

    #[test]
    fn credential_selector_options_include_supported_credentials() {
        let options = credential_options();

        assert_eq!(
            options
                .iter()
                .map(|(label, _, _)| label.as_str())
                .collect::<Vec<_>>(),
            vec![SSL_CLOUDFLARE_CREDENTIAL_NAME, POSTGRES_CREDENTIAL_NAME]
        );
        assert_eq!(
            options
                .into_iter()
                .map(|(_, credential, _)| credential)
                .collect::<Vec<_>>(),
            vec![
                SSL_CLOUDFLARE_CREDENTIAL_NAME.to_string(),
                POSTGRES_CREDENTIAL_NAME.to_string()
            ]
        );
    }

    #[test]
    fn credential_status_rows_include_supported_credentials_and_config_envs() {
        let mut config = crate::config::TakoToml::default();
        config.envs.insert(
            "development".to_string(),
            crate::config::EnvConfig::default(),
        );
        config.envs.insert(
            "production".to_string(),
            crate::config::EnvConfig::default(),
        );
        config
            .envs
            .insert("staging".to_string(), crate::config::EnvConfig::default());

        let mut secrets = crate::config::SecretsStore::default();
        secrets.ensure_env_key_id("production").unwrap();
        secrets
            .set_credential(
                "production",
                SSL_CLOUDFLARE_CREDENTIAL_NAME,
                EncryptedSecretValue::new("encrypted-token".to_string(), None),
            )
            .unwrap();

        let (envs, rows) = credential_status_rows(&config, &secrets);

        assert_eq!(envs, vec!["production", "staging"]);
        assert_eq!(
            rows,
            vec![
                CredentialStatusRow {
                    name: POSTGRES_CREDENTIAL_NAME.to_string(),
                    envs: vec![
                        ("production".to_string(), false),
                        ("staging".to_string(), false)
                    ]
                },
                CredentialStatusRow {
                    name: SSL_CLOUDFLARE_CREDENTIAL_NAME.to_string(),
                    envs: vec![
                        ("production".to_string(), true),
                        ("staging".to_string(), false)
                    ]
                },
            ]
        );
    }

    #[test]
    fn credential_environment_options_exclude_development_and_new_environment() {
        let mut config = crate::config::TakoToml::default();
        config.envs.insert(
            "development".to_string(),
            crate::config::EnvConfig::default(),
        );
        config
            .envs
            .insert("staging".to_string(), crate::config::EnvConfig::default());
        let mut secrets = crate::config::SecretsStore::default();
        secrets.ensure_env_key_id("qa").unwrap();

        let options = credential_environment_options(&config, &secrets);

        assert_eq!(
            options,
            vec![
                ("production".to_string(), "production".to_string()),
                ("qa".to_string(), "qa".to_string()),
                ("staging".to_string(), "staging".to_string())
            ]
        );
    }

    #[test]
    fn requested_credential_environment_rejects_development() {
        let err = validate_requested_credential_environment("development").unwrap_err();

        assert_eq!(
            err.to_string(),
            "Provider credentials are for deployed environments. Use a deployment environment like production."
        );
    }

    #[test]
    fn missing_credential_name_requires_interactive_selector() {
        let err = resolve_credential_name(None).unwrap_err();

        assert_eq!(
            err.to_string(),
            "Missing required credential name. Run `tako credentials set <name>` or run interactively to choose one."
        );
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

    #[test]
    fn decrypt_runtime_credentials_only_includes_postgres_url() {
        with_temp_tako_home(|| {
            let project = tempfile::TempDir::new().unwrap();
            set_credential_value(
                project.path(),
                "production",
                SSL_CLOUDFLARE_CREDENTIAL_NAME,
                "cloudflare-token",
                None,
            )
            .unwrap();
            set_credential_value(
                project.path(),
                "production",
                POSTGRES_CREDENTIAL_NAME,
                "postgres://runtime",
                None,
            )
            .unwrap();

            let secrets = crate::config::SecretsStore::load_from_dir(project.path()).unwrap();
            let runtime =
                decrypt_runtime_credentials("production", &secrets, Some(project.path())).unwrap();

            assert_eq!(runtime.len(), 1);
            assert_eq!(
                runtime.get(POSTGRES_CREDENTIAL_NAME).map(String::as_str),
                Some("postgres://runtime")
            );
        });
    }

    #[test]
    fn letsencrypt_wildcard_accepts_cloudflare_account_api_tokens() {
        validate_cloudflare_token_for_ssl_binding(
            tako_core::SslProvider::LetsEncrypt,
            &["*.example.com".to_string()],
            "cfat_test_account_token",
        )
        .unwrap();
    }

    #[test]
    fn cloudflare_ssl_accepts_cloudflare_account_api_tokens() {
        validate_cloudflare_token_for_ssl_binding(
            tako_core::SslProvider::Cloudflare,
            &["*.example.com".to_string()],
            "cfat_test_account_token",
        )
        .unwrap();
    }
}
