use crate::build::{self, PresetGroup, detect_build_adapter};
use crate::config::TakoToml;
use crate::output;
use clap::Subcommand;
use std::collections::BTreeSet;
use std::path::Path;

mod key;
mod sync;
#[cfg(test)]
mod tests;

pub(crate) use key::load_or_create_key_for_set;
pub use key::{ensure_secret_key_available, load_secret_key};
use key::{export_key, import_key};
use sync::{list_secrets, sync_secrets};

/// Refresh generated files (`tako.d.ts` for JS/TS, `tako_secrets.go` for Go)
/// after a secret change. Best-effort — a generation failure doesn't block
/// the secret write itself.
fn refresh_generated_files_after_secret_change(project_dir: &Path, config_path: &Path) {
    let tako_config = match TakoToml::load_from_file(config_path) {
        Ok(cfg) => cfg,
        Err(_) => return,
    };
    let adapter = tako_config
        .runtime
        .as_deref()
        .map(str::trim)
        .filter(|v: &&str| !v.is_empty())
        .and_then(build::BuildAdapter::from_id)
        .unwrap_or_else(|| detect_build_adapter(project_dir));
    match adapter.preset_group() {
        PresetGroup::Js => {
            let _ = build::js::write_tako_declarations_for_adapter_and_app_root(
                project_dir,
                adapter,
                tako_config.js_app_root(),
            );
        }
        PresetGroup::Go => {
            let _ = build::go::write_secret_accessors(project_dir);
        }
        PresetGroup::Unknown => {}
    }
}

#[derive(Subcommand)]
pub enum SecretCommands {
    /// Set a secret (creates or overwrites)
    #[command(visible_alias = "add")]
    Set {
        /// Secret name (uppercase, underscores)
        name: String,

        /// Environment to set the secret for
        #[arg(long)]
        env: Option<String>,

        /// Optional expiry date. Use YYYY-MM-DD, "in 30 days", or never.
        #[arg(long)]
        expires_on: Option<String>,

        /// Sync secrets to servers after setting
        #[arg(long)]
        sync: bool,
    },

    /// Remove a secret
    #[command(visible_aliases = ["remove", "delete", "del"])]
    Rm {
        /// Secret name
        name: String,

        /// Environment to remove from (or all if not specified)
        #[arg(long)]
        env: Option<String>,

        /// Sync secrets to servers after removing
        #[arg(long)]
        sync: bool,
    },

    /// List all secrets
    #[command(visible_aliases = ["ls", "show"])]
    List,

    /// Sync secrets to servers
    Sync {
        /// Only sync to specific environment
        #[arg(long)]
        env: Option<String>,
    },

    /// Manage encryption keys used for secrets
    #[command(subcommand)]
    Key(SecretKeyCommands),
}

#[derive(Subcommand)]
pub enum SecretKeyCommands {
    /// Export a self-contained key bundle and copy it to clipboard
    Export {
        /// Target environment key
        #[arg(long)]
        env: Option<String>,
    },

    /// Import an exported key or passphrase from a prompt or stdin
    Import {
        /// Import a passphrase-derived key
        #[arg(long)]
        passphrase: bool,

        /// Target environment for key import
        #[arg(long)]
        env: Option<String>,
    },
}

pub fn run(
    cmd: SecretCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = crate::commands::project_context::resolve_existing(config_path)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd, context))
}

fn read_secret_value(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::{IsTerminal, Read};

    if std::io::stdin().is_terminal() {
        return Ok(secret_value_field(prompt).prompt()?);
    }

    // Non-interactive fallback for CI/piped input.
    let mut value = String::new();
    let bytes = std::io::stdin().read_to_string(&mut value)?;
    if bytes == 0 {
        return Err("No secret value provided on stdin".into());
    }
    let value = value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .or_else(|| value.strip_suffix('\r'))
        .unwrap_or(&value)
        .to_string();
    if value.is_empty() {
        return Err("Secret value cannot be empty".into());
    }

    Ok(value)
}

fn secret_value_prompt_hint() -> &'static str {
    "Multiline paste supported."
}

fn secret_value_field(prompt: &str) -> output::TextField<'_> {
    output::TextField::new(prompt)
        .password()
        .with_hint(secret_value_prompt_hint())
}

pub(crate) fn read_secret_expires_on(
    value: Option<String>,
    prompt: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if let Some(value) = value {
        return crate::config::normalize_secret_expires_on(&value).map_err(Into::into);
    }

    if !output::is_interactive() {
        return Ok(None);
    }

    let raw = output::TextField::new(prompt)
        .with_hint(crate::config::secret_expires_on_prompt_hint())
        .prompt_validated(|value| {
            crate::config::normalize_secret_expires_on(value)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })?;
    crate::config::normalize_secret_expires_on(&raw).map_err(Into::into)
}

async fn run_async(
    cmd: SecretCommands,
    context: crate::commands::project_context::ProjectContext,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SecretCommands::Set {
            name,
            env,
            expires_on,
            sync,
        } => {
            let Some(input) =
                resolve_secret_set_input(&context, env.as_deref(), &name, expires_on)?
            else {
                return Ok(());
            };
            set_secret(
                &context,
                &name,
                &input.env,
                &input.value,
                input.expires_on,
                sync,
            )
            .await
        }
        SecretCommands::Rm { name, env, sync } => {
            remove_secret(&context, &name, env.as_deref(), sync).await
        }
        SecretCommands::List => list_secrets(&context).await,
        SecretCommands::Sync { env } => sync_secrets(&context, env.as_deref()).await,
        SecretCommands::Key(SecretKeyCommands::Export { env }) => {
            let env = resolve_secret_environment(&context, env.as_deref(), "Key environment")?;
            export_key(&context, &env).await
        }
        SecretCommands::Key(SecretKeyCommands::Import { passphrase, env }) => {
            import_key(&context, passphrase, env.as_deref()).await
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SecretEnvironmentChoice {
    Existing(String),
    New,
}

struct SecretSetInput {
    env: String,
    value: String,
    expires_on: Option<String>,
}

fn secret_environment_options(
    tako_config: &crate::config::TakoToml,
    secrets: &crate::config::SecretsStore,
) -> Vec<(String, SecretEnvironmentChoice)> {
    let mut names = BTreeSet::new();
    names.extend(tako_config.get_environment_names());
    names.extend(secrets.environment_names());

    let mut options = Vec::new();
    for default_env in ["development", "production"] {
        names.remove(default_env);
        options.push((
            default_env.to_string(),
            SecretEnvironmentChoice::Existing(default_env.to_string()),
        ));
    }
    options.extend(
        names
            .into_iter()
            .map(|name| (name.clone(), SecretEnvironmentChoice::Existing(name))),
    );
    options.push(("New environment".to_string(), SecretEnvironmentChoice::New));
    options
}

pub(crate) fn resolve_secret_environment(
    context: &crate::commands::project_context::ProjectContext,
    requested: Option<&str>,
    label: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(env) = requested {
        crate::config::validate_environment_name(env)?;
        return Ok(env.to_string());
    }

    if !output::is_interactive() {
        return Err(
            "Missing required environment. Pass --env or run interactively to choose one.".into(),
        );
    }

    let tako_config = crate::config::TakoToml::load_from_file(&context.config_path)?;
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let mut wizard = output::Wizard::new().with_fields(&[("Environment", false), ("Name", true)]);

    let choice = wizard.select(
        "Environment",
        label,
        secret_environment_options(&tako_config, &secrets),
        &[],
        0,
    )?;
    match choice {
        SecretEnvironmentChoice::Existing(env) => Ok(env),
        SecretEnvironmentChoice::New => loop {
            wizard.set_visible("Name", true);
            let name = wizard.input(
                "Name",
                None,
                Some("Use lowercase letters, numbers, and hyphens."),
            )?;
            match crate::config::validate_environment_name(&name) {
                Ok(()) => return Ok(name),
                Err(e) => {
                    output::warning(&e.to_string());
                    wizard.undo_last();
                }
            }
        },
    }
}

fn secret_value_prompt(secrets: &crate::config::SecretsStore, name: &str, env: &str) -> String {
    if secrets.contains(env, name) {
        "Enter new value".to_string()
    } else {
        format!("Enter value for {}", name)
    }
}

fn replace_existing_value_prompt() -> &'static str {
    // CodeQL[rust/cleartext-logging]: confirm prompts are written to stderr, so keep secret names out.
    "Value is already set. Replace it?"
}

fn confirm_secret_override(
    secrets: &crate::config::SecretsStore,
    name: &str,
    env: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    if !secrets.contains(env, name) || !output::is_interactive() {
        return Ok(true);
    }

    let confirmed = match output::confirm(replace_existing_value_prompt(), false) {
        Ok(confirmed) => confirmed,
        Err(e) if output::is_wizard_back(&e) => false,
        Err(e) => return Err(e.into()),
    };

    if !confirmed {
        output::operation_cancelled();
    }

    Ok(confirmed)
}

fn read_secret_value_in_wizard(
    wizard: &mut output::Wizard,
    secrets: &crate::config::SecretsStore,
    name: &str,
    env: &str,
) -> std::io::Result<String> {
    let prompt = secret_value_prompt(secrets, name, env);
    wizard.text_field_named("Value", secret_value_field(&prompt))
}

fn read_secret_expires_on_in_wizard(
    wizard: &mut output::Wizard,
) -> std::io::Result<Option<String>> {
    let raw = wizard.text_field_named_validated_with_spinner(
        "Expires",
        output::TextField::new("Expires on")
            .with_hint(crate::config::secret_expires_on_prompt_hint()),
        |value| {
            crate::config::normalize_secret_expires_on(&value)
                .map(|_| ())
                .map_err(|e| e.to_string())
        },
    )?;
    crate::config::normalize_secret_expires_on(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))
}

fn read_secret_value_and_expires_on_in_wizard(
    wizard: &mut output::Wizard,
    secrets: &crate::config::SecretsStore,
    name: &str,
    env: &str,
    requested_expires_on: Option<Option<String>>,
) -> std::io::Result<(String, Option<String>)> {
    loop {
        let value = read_secret_value_in_wizard(wizard, secrets, name, env)?;
        if let Some(expires_on) = &requested_expires_on {
            return Ok((value, expires_on.clone()));
        }
        match read_secret_expires_on_in_wizard(wizard) {
            Ok(expires_on) => return Ok((value, expires_on)),
            Err(e) if output::is_wizard_back(&e) => {
                wizard.undo_last();
            }
            Err(e) => return Err(e),
        }
    }
}

fn resolve_secret_set_input(
    context: &crate::commands::project_context::ProjectContext,
    requested_env: Option<&str>,
    name: &str,
    requested_expires_on: Option<String>,
) -> Result<Option<SecretSetInput>, Box<dyn std::error::Error>> {
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let requested_expires_on = requested_expires_on
        .map(|value| crate::config::normalize_secret_expires_on(&value))
        .transpose()?;

    if let Some(env) = requested_env {
        crate::config::validate_environment_name(env)?;
        if !confirm_secret_override(&secrets, name, env)? {
            return Ok(None);
        }
        let prompt = secret_value_prompt(&secrets, name, env);
        if requested_expires_on.is_none() && output::is_interactive() {
            let value = read_secret_value(&prompt)?;
            let expires_on = read_secret_expires_on(None, "Expires on")?;
            return Ok(Some(SecretSetInput {
                env: env.to_string(),
                value,
                expires_on,
            }));
        }
        let expires_on = match requested_expires_on {
            Some(expires_on) => expires_on,
            None => read_secret_expires_on(None, "Expires on")?,
        };
        return Ok(Some(SecretSetInput {
            env: env.to_string(),
            value: read_secret_value(&prompt)?,
            expires_on,
        }));
    }

    if !output::is_interactive() {
        return Err(
            "Missing required environment. Pass --env or run interactively to choose one.".into(),
        );
    }

    let tako_config = crate::config::TakoToml::load_from_file(&context.config_path)?;
    let prompt_for_expires_on = requested_expires_on.is_none();
    let mut wizard = output::Wizard::new().with_fields(&[
        ("Environment", false),
        ("Name", true),
        ("Override", true),
        ("Value", false),
        ("Expires", false),
    ]);

    'environment: loop {
        wizard.set_visible("Name", false);
        wizard.set_visible("Override", false);
        wizard.set_visible("Value", false);
        wizard.set_visible("Expires", false);
        wizard.set_visible("Value", true);
        wizard.set_visible("Expires", prompt_for_expires_on);
        let choice = wizard.select(
            "Environment",
            "Secret environment",
            secret_environment_options(&tako_config, &secrets),
            &[],
            0,
        )?;

        match choice {
            SecretEnvironmentChoice::Existing(env) => {
                if secrets.contains(&env, name) {
                    wizard.set_visible("Override", true);
                    'override_existing: loop {
                        match wizard.confirm_default(
                            "Override",
                            replace_existing_value_prompt(),
                            false,
                        ) {
                            Ok(true) => {}
                            Ok(false) => {
                                output::operation_cancelled();
                                return Ok(None);
                            }
                            Err(e) if output::is_wizard_back(&e) => {
                                wizard.undo_last();
                                continue 'environment;
                            }
                            Err(e) => return Err(e.into()),
                        }

                        match read_secret_value_and_expires_on_in_wizard(
                            &mut wizard,
                            &secrets,
                            name,
                            &env,
                            requested_expires_on.clone(),
                        ) {
                            Ok((value, expires_on)) => {
                                return Ok(Some(SecretSetInput {
                                    env,
                                    value,
                                    expires_on,
                                }));
                            }
                            Err(e) if output::is_wizard_back(&e) => {
                                wizard.undo_last();
                                continue 'override_existing;
                            }
                            Err(e) => return Err(e.into()),
                        }
                    }
                }

                match read_secret_value_and_expires_on_in_wizard(
                    &mut wizard,
                    &secrets,
                    name,
                    &env,
                    requested_expires_on.clone(),
                ) {
                    Ok((value, expires_on)) => {
                        return Ok(Some(SecretSetInput {
                            env,
                            value,
                            expires_on,
                        }));
                    }
                    Err(e) if output::is_wizard_back(&e) => {
                        wizard.undo_last();
                        continue 'environment;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            SecretEnvironmentChoice::New => {
                wizard.set_visible("Name", true);
                'name: loop {
                    let env = match wizard.input(
                        "Name",
                        None,
                        Some("Use lowercase letters, numbers, and hyphens."),
                    ) {
                        Ok(env) => env,
                        Err(e) if output::is_wizard_back(&e) => {
                            wizard.undo_last();
                            continue 'environment;
                        }
                        Err(e) => return Err(e.into()),
                    };

                    if let Err(e) = crate::config::validate_environment_name(&env) {
                        output::warning(&e.to_string());
                        wizard.undo_last();
                        continue 'name;
                    }

                    if secrets.contains(&env, name) {
                        wizard.set_visible("Override", true);
                        'override_new: loop {
                            match wizard.confirm_default(
                                "Override",
                                replace_existing_value_prompt(),
                                false,
                            ) {
                                Ok(true) => {}
                                Ok(false) => {
                                    output::operation_cancelled();
                                    return Ok(None);
                                }
                                Err(e) if output::is_wizard_back(&e) => {
                                    wizard.undo_last();
                                    continue 'name;
                                }
                                Err(e) => return Err(e.into()),
                            }

                            match read_secret_value_and_expires_on_in_wizard(
                                &mut wizard,
                                &secrets,
                                name,
                                &env,
                                requested_expires_on.clone(),
                            ) {
                                Ok((value, expires_on)) => {
                                    return Ok(Some(SecretSetInput {
                                        env,
                                        value,
                                        expires_on,
                                    }));
                                }
                                Err(e) if output::is_wizard_back(&e) => {
                                    wizard.undo_last();
                                    continue 'override_new;
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                    }

                    match read_secret_value_and_expires_on_in_wizard(
                        &mut wizard,
                        &secrets,
                        name,
                        &env,
                        requested_expires_on.clone(),
                    ) {
                        Ok((value, expires_on)) => {
                            return Ok(Some(SecretSetInput {
                                env,
                                value,
                                expires_on,
                            }));
                        }
                        Err(e) if output::is_wizard_back(&e) => {
                            wizard.undo_last();
                            continue 'name;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
    }
}

async fn set_secret(
    context: &crate::commands::project_context::ProjectContext,
    name: &str,
    env: &str,
    value: &str,
    expires_on: Option<String>,
    do_sync: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::SecretsStore;
    use crate::crypto::encrypt;

    // Load secrets and ensure environment has an in-memory key_id. Do not write
    // secrets.json until every prompt in this flow has completed.
    let mut secrets = SecretsStore::load_from_dir(&context.project_dir)?;
    secrets.ensure_env_key_id(env)?;

    let exists = secrets.contains(env, name);

    // Get or create the local encryption key only after the value prompt
    // succeeds, so cancelled wizards do not touch secrets.json or key files.
    let key = load_or_create_key_for_set(env, &secrets, Some(&context.project_dir))?;

    // Encrypt and store
    let encrypted = encrypt(value, &key)?;
    secrets.set_with_expires_on(env, name, encrypted, expires_on)?;
    secrets.save_to_dir(&context.project_dir)?;
    refresh_generated_files_after_secret_change(&context.project_dir, &context.config_path);

    if exists {
        output::success(&format!(
            "Updated {} in {}",
            output::strong(name),
            output::strong(env)
        ));
    } else {
        output::success(&format!(
            "Set {} in {}",
            output::strong(name),
            output::strong(env)
        ));
    }

    if do_sync {
        sync_secrets(context, Some(env)).await?;
    }

    Ok(())
}

async fn remove_secret(
    context: &crate::commands::project_context::ProjectContext,
    name: &str,
    env: Option<&str>,
    do_sync: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::SecretsStore;
    let mut secrets = SecretsStore::load_from_dir(&context.project_dir)?;

    if let Some(env) = env {
        // Remove from specific environment
        if !secrets.contains(env, name) {
            return Err(format!("Secret '{}' not found in environment '{}'", name, env).into());
        }

        let confirm = crate::output::confirm(
            &format!(
                "Remove secret {} from {}?",
                output::strong(name),
                output::strong(env)
            ),
            false,
        )?;

        if !confirm {
            output::operation_cancelled();
            return Ok(());
        }

        secrets.remove(env, name)?;
        output::success(&format!(
            "Removed secret {} from environment {}",
            output::strong(name),
            output::strong(env)
        ));
    } else {
        // Remove from all environments
        let confirm = crate::output::confirm(
            &format!(
                "Remove secret {} from ALL environments?",
                output::strong(name)
            ),
            false,
        )?;

        if !confirm {
            output::operation_cancelled();
            return Ok(());
        }

        let removed_from = secrets.remove_all(name)?;
        output::success(&format!(
            "Removed secret {} from environments: {}",
            output::strong(name),
            removed_from.join(", ")
        ));
    }

    secrets.save_to_dir(&context.project_dir)?;
    refresh_generated_files_after_secret_change(&context.project_dir, &context.config_path);

    if do_sync {
        // Sync to the specific env if provided, otherwise all environments
        sync_secrets(context, env).await?;
    }

    Ok(())
}
