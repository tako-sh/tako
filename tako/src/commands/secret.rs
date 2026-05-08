use crate::build::{self, PresetGroup, detect_build_adapter};
use crate::config::TakoToml;
use crate::output;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use tako_core::Command;

/// Regenerate typed accessors (`tako.gen.ts` for JS/TS, `tako_secrets.go` for
/// Go) after a secret change. Best-effort — a typegen failure doesn't block
/// the secret write itself.
fn regenerate_types_after_secret_change(project_dir: &Path, config_path: &Path) {
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
            let _ = build::js::write_types_for_adapter(project_dir, adapter);
        }
        PresetGroup::Go => {
            let _ = build::go::write_types(project_dir);
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
    #[command(visible_aliases = ["list", "show"])]
    Ls,

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
    use std::io::IsTerminal;

    if std::io::stdin().is_terminal() {
        return Ok(crate::output::password_field(prompt)?);
    }

    // Non-interactive fallback for CI/piped input.
    let mut value = String::new();
    let bytes = std::io::stdin().read_line(&mut value)?;
    if bytes == 0 {
        return Err("No secret value provided on stdin".into());
    }
    let value = value.trim_end_matches(['\r', '\n']).to_string();
    if value.is_empty() {
        return Err("Secret value cannot be empty".into());
    }

    Ok(value)
}

fn read_key_bundle() -> Result<String, Box<dyn std::error::Error>> {
    use std::io::IsTerminal;

    let value = if std::io::stdin().is_terminal() {
        crate::output::password_field("Exported key")?
    } else {
        let mut value = String::new();
        let bytes = std::io::stdin().read_line(&mut value)?;
        if bytes == 0 {
            return Err("No exported key provided on stdin".into());
        }
        value.trim_end_matches(['\r', '\n']).to_string()
    };

    if value.trim().is_empty() {
        return Err("Exported key cannot be empty".into());
    }

    Ok(value.trim().to_string())
}

const INVALID_PASSPHRASE_PROMPT_ERROR: &str = "Invalid passphrase";
const INVALID_PASSPHRASE_ERROR: &str = "Invalid passphrase.";

fn read_passphrase_key_for_env(
    secrets: &crate::config::SecretsStore,
    env: &str,
    key_id: &str,
) -> Result<crate::crypto::EncryptionKey, Box<dyn std::error::Error>> {
    use std::io::IsTerminal;

    let passphrase = if std::io::stdin().is_terminal() {
        crate::output::TextField::new("Passphrase")
            .password()
            .prompt_validated(|value| {
                let key = crate::crypto::derive_key_from_passphrase(value, key_id)
                    .map_err(|_| INVALID_PASSPHRASE_PROMPT_ERROR.to_string())?;
                validate_passphrase_key_for_env(secrets, env, &key)
                    .map_err(|_| INVALID_PASSPHRASE_PROMPT_ERROR.to_string())
            })?
    } else {
        let mut value = String::new();
        let bytes = std::io::stdin().read_line(&mut value)?;
        if bytes == 0 {
            return Err("No passphrase provided on stdin".into());
        }
        value.trim_end_matches(['\r', '\n']).to_string()
    };

    if passphrase.is_empty() {
        return Err("Passphrase cannot be empty.".into());
    }

    let key = crate::crypto::derive_key_from_passphrase(&passphrase, key_id)?;
    validate_passphrase_key_for_env(secrets, env, &key)?;

    Ok(key)
}

async fn run_async(
    cmd: SecretCommands,
    context: crate::commands::project_context::ProjectContext,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SecretCommands::Set { name, env, sync } => {
            let Some(input) = resolve_secret_set_input(&context, env.as_deref(), &name)? else {
                return Ok(());
            };
            set_secret(&context, &name, &input.env, &input.value, sync).await
        }
        SecretCommands::Rm { name, env, sync } => {
            remove_secret(&context, &name, env.as_deref(), sync).await
        }
        SecretCommands::Ls => list_secrets(&context).await,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyImportSource {
    ExportedKey,
    Passphrase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyStorageChoice {
    LocalFile,
    #[cfg(target_os = "macos")]
    ICloudKeychain,
}

struct SecretSetInput {
    env: String,
    value: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ExportedKeyBundle {
    version: u8,
    id: String,
    key: String,
}

fn encode_key_bundle(key_id: &str, key: &crate::crypto::EncryptionKey) -> String {
    let bundle = ExportedKeyBundle {
        version: 1,
        id: key_id.to_string(),
        key: key.to_base64(),
    };
    let json = serde_json::to_vec(&bundle).expect("serialize key bundle");
    BASE64_URL.encode(json)
}

fn decode_key_bundle(
    input: &str,
) -> Result<(String, crate::crypto::EncryptionKey), Box<dyn std::error::Error>> {
    let trimmed = input.trim();
    let json = BASE64_URL
        .decode(trimmed)
        .map_err(|e| format!("Invalid key payload: {e}"))?;
    let bundle: ExportedKeyBundle =
        serde_json::from_slice(&json).map_err(|e| format!("Invalid key payload: {e}"))?;
    if bundle.version != 1 {
        return Err(format!("Unsupported key version: {}", bundle.version).into());
    }
    crate::crypto::KeyStore::for_key_id(&bundle.id)?;
    let key = crate::crypto::EncryptionKey::from_base64(&bundle.key)?;
    Ok((bundle.id, key))
}

fn resolve_key_import_source(
    passphrase: bool,
) -> Result<KeyImportSource, Box<dyn std::error::Error>> {
    match passphrase {
        true => Ok(KeyImportSource::Passphrase),
        false if output::is_interactive() => Ok(output::select(
            "Key source",
            None,
            vec![
                ("Exported key".to_string(), KeyImportSource::ExportedKey),
                ("Passphrase".to_string(), KeyImportSource::Passphrase),
            ],
        )?),
        false => Ok(KeyImportSource::ExportedKey),
    }
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

fn resolve_secret_environment(
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
    wizard.text_field_named("Value", output::TextField::new(&prompt).password())
}

fn resolve_secret_set_input(
    context: &crate::commands::project_context::ProjectContext,
    requested_env: Option<&str>,
    name: &str,
) -> Result<Option<SecretSetInput>, Box<dyn std::error::Error>> {
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;

    if let Some(env) = requested_env {
        crate::config::validate_environment_name(env)?;
        if !confirm_secret_override(&secrets, name, env)? {
            return Ok(None);
        }
        let prompt = secret_value_prompt(&secrets, name, env);
        return Ok(Some(SecretSetInput {
            env: env.to_string(),
            value: read_secret_value(&prompt)?,
        }));
    }

    if !output::is_interactive() {
        return Err(
            "Missing required environment. Pass --env or run interactively to choose one.".into(),
        );
    }

    let tako_config = crate::config::TakoToml::load_from_file(&context.config_path)?;
    let mut wizard = output::Wizard::new().with_fields(&[
        ("Environment", false),
        ("Name", true),
        ("Override", true),
        ("Value", false),
    ]);

    'environment: loop {
        wizard.set_visible("Name", false);
        wizard.set_visible("Override", false);
        wizard.set_visible("Value", false);
        wizard.set_visible("Value", true);
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

                        match read_secret_value_in_wizard(&mut wizard, &secrets, name, &env) {
                            Ok(value) => {
                                return Ok(Some(SecretSetInput { env, value }));
                            }
                            Err(e) if output::is_wizard_back(&e) => {
                                wizard.undo_last();
                                continue 'override_existing;
                            }
                            Err(e) => return Err(e.into()),
                        }
                    }
                }

                match read_secret_value_in_wizard(&mut wizard, &secrets, name, &env) {
                    Ok(value) => {
                        return Ok(Some(SecretSetInput { env, value }));
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

                            match read_secret_value_in_wizard(&mut wizard, &secrets, name, &env) {
                                Ok(value) => {
                                    return Ok(Some(SecretSetInput { env, value }));
                                }
                                Err(e) if output::is_wizard_back(&e) => {
                                    wizard.undo_last();
                                    continue 'override_new;
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                    }

                    match read_secret_value_in_wizard(&mut wizard, &secrets, name, &env) {
                        Ok(value) => {
                            return Ok(Some(SecretSetInput { env, value }));
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

fn environment_for_key_id(secrets: &crate::config::SecretsStore, key_id: &str) -> Option<String> {
    secrets
        .environment_names()
        .into_iter()
        .find(|env| secrets.get_key_id(env) == Some(key_id))
}

async fn set_secret(
    context: &crate::commands::project_context::ProjectContext,
    name: &str,
    env: &str,
    value: &str,
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
    secrets.set(env, name, encrypted)?;
    secrets.save_to_dir(&context.project_dir)?;
    regenerate_types_after_secret_change(&context.project_dir, &context.config_path);

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
    regenerate_types_after_secret_change(&context.project_dir, &context.config_path);

    if do_sync {
        // Sync to the specific env if provided, otherwise all environments
        sync_secrets(context, env).await?;
    }

    Ok(())
}

async fn list_secrets(
    context: &crate::commands::project_context::ProjectContext,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::SecretsStore;
    let secrets = SecretsStore::load_from_dir(&context.project_dir)?;

    if secrets.is_empty() {
        output::warning("No secrets configured.");
        output::muted(&format!(
            "Run {} to add a secret.",
            output::strong("tako secrets set")
        ));
        return Ok(());
    }

    output::section("Secrets");

    let all_names = secrets.all_secret_names();
    let all_envs = secrets.environment_names();

    let discrepancies = secrets.find_discrepancies();

    if output::is_pretty() {
        // Print header
        eprint!("{:<30}", "SECRET");
        for env in &all_envs {
            eprint!(" {:<15}", env.to_uppercase());
        }
        eprintln!();

        eprint!("{}", "-".repeat(30));
        for _ in &all_envs {
            eprint!(" {}", "-".repeat(15));
        }
        eprintln!();

        // Print each secret
        let discrepancy_names: Vec<&str> = discrepancies.iter().map(|d| d.name.as_str()).collect();

        for name in &all_names {
            eprint!("{:<30}", name);
            for env in &all_envs {
                if secrets.contains(env, name) {
                    eprint!(" {:<15}", "[set]");
                } else {
                    eprint!(" {:<15}", "-");
                }
            }

            // Show warning if this secret has discrepancies
            if discrepancy_names.contains(&name.as_str()) {
                eprint!(" (missing in some envs)");
            }

            eprintln!();
        }
    } else {
        for name in &all_names {
            let envs_with_secret: Vec<&str> = all_envs
                .iter()
                .filter(|env| secrets.contains(env, name))
                .map(|s| s.as_str())
                .collect();
            tracing::info!("{name}: set in {}", envs_with_secret.join(", "));
        }
    }

    // Summary
    if !discrepancies.is_empty() {
        output::warning(&format!(
            "{} secret(s) have discrepancies across environments.",
            output::strong(&discrepancies.len().to_string())
        ));
        output::muted(&format!(
            "Run {} to sync secrets to servers.",
            output::strong("tako secrets sync")
        ));
    }

    Ok(())
}

async fn sync_secrets(
    context: &crate::commands::project_context::ProjectContext,
    target_env: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::{SecretsStore, ServersToml, TakoToml};
    use crate::crypto::decrypt;
    let app_name = resolve_app_name(&context.config_path)?;
    let secrets = SecretsStore::load_from_dir(&context.project_dir)?;
    let tako_config = TakoToml::load_from_file(&context.config_path)?;
    let mut servers = ServersToml::load()?;

    if secrets.is_empty() {
        output::warning("No secrets to sync.");
        return Ok(());
    }

    if servers.is_empty()
        && super::server::prompt_to_add_server(
            "No servers configured yet. Add one now to sync secrets.",
        )
        .await?
        .is_some()
    {
        servers = ServersToml::load()?;
    }

    // Check for discrepancies first
    let discrepancies = secrets.find_discrepancies();
    if !discrepancies.is_empty() {
        output::warning("Some secrets are missing in certain environments:");
        for d in &discrepancies {
            output::warning(&format!(
                "{} missing in: {}",
                output::strong(&d.name),
                d.missing_in.join(", ")
            ));
        }
    }

    // Determine which environments to sync
    let envs_to_sync: Vec<String> = if let Some(env) = target_env {
        if !tako_config.envs.contains_key(env) {
            return Err(format!("Environment '{}' not found in tako.toml", env).into());
        }
        vec![env.to_string()]
    } else {
        tako_config.get_environment_names()
    };

    // Collect all (env, server_name, server_entry) targets first
    let mut sync_targets: Vec<(String, String, crate::config::ServerEntry)> = Vec::new();
    for env_name in &envs_to_sync {
        let server_names = resolve_secret_sync_server_names(env_name, &tako_config, &servers)
            .map_err(|e| {
                format!(
                    "Failed to resolve target servers for environment '{}': {}",
                    env_name, e
                )
            })?;

        if server_names.is_empty() {
            output::warning(&format!(
                "Skipping {} — no servers configured",
                output::strong(env_name)
            ));
            continue;
        }

        for server_name in server_names {
            let server = match servers.get(server_name.as_str()) {
                Some(s) => s.clone(),
                None => {
                    output::error(&format!(
                        "{} — server not found",
                        output::strong(&server_name)
                    ));
                    continue;
                }
            };
            sync_targets.push((env_name.clone(), server_name, server));
        }
    }

    if sync_targets.is_empty() {
        output::warning("No servers to sync to.");
        return Ok(());
    }

    let total_servers = sync_targets.len();
    let spinner =
        output::TrackedSpinner::start(&format!("Syncing secrets to {total_servers} server(s)…"));
    let sync_start = std::time::Instant::now();

    let mut success_count = 0;
    let mut error_count = 0;

    for (env_name, server_name, server) in &sync_targets {
        let _scope = output::scope(server_name).entered();
        let _t = output::timed(&format!("Sync secrets ({env_name})"));
        // Get decrypted secrets for this environment
        let env_secrets = match secrets.get_env(env_name) {
            Some(encrypted_secrets) => {
                let key = load_secret_key(env_name, &secrets, Some(&context.project_dir))?;
                let mut decrypted = std::collections::HashMap::new();
                for (name, encrypted_value) in encrypted_secrets {
                    match decrypt(encrypted_value, &key) {
                        Ok(value) => {
                            decrypted.insert(name.clone(), value);
                        }
                        Err(e) => {
                            output::warning(&format!(
                                "Failed to decrypt {}: {}",
                                output::strong(name),
                                e
                            ));
                        }
                    }
                }
                decrypted
            }
            None => {
                output::warning(&format!(
                    "No secrets for environment {}",
                    output::strong(env_name)
                ));
                continue;
            }
        };

        if env_secrets.is_empty() {
            continue;
        }

        let remote_app_name = tako_core::deployment_app_id(&app_name, env_name);
        match sync_to_server(&remote_app_name, server, &env_secrets).await {
            Ok(()) => {
                tracing::debug!("Synced {} secret(s) for {env_name}", env_secrets.len());
                success_count += 1;
            }
            Err(e) => {
                output::error(&format!("{} ({})", e, output::strong(server_name)));
                error_count += 1;
            }
        }
    }

    let elapsed = sync_start.elapsed();
    spinner.finish();

    if error_count == 0 {
        output::success(&format!(
            "Synced secrets to {} server(s) ({:.1}s)",
            output::strong(&success_count.to_string()),
            elapsed.as_secs_f64()
        ));
    } else {
        output::warning(&format!(
            "Synced to {} server(s), {} failed ({:.1}s)",
            output::strong(&success_count.to_string()),
            output::strong(&error_count.to_string()),
            elapsed.as_secs_f64()
        ));
    }

    Ok(())
}

fn resolve_secret_sync_server_names(
    env_name: &str,
    tako_config: &crate::config::TakoToml,
    servers: &crate::config::ServersToml,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut resolved = match super::helpers::resolve_servers_for_env(tako_config, servers, env_name)
    {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };
    resolved.sort();
    resolved.dedup();
    Ok(resolved)
}

async fn export_key(
    context: &crate::commands::project_context::ProjectContext,
    env: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let key_id = secrets
        .get_key_id(env)
        .ok_or_else(|| format!("No secrets configured for environment '{}'.", env))?;
    let key_store = crate::crypto::KeyStore::for_key_id(key_id)?;
    let Some(key) = key_store.load_key_optional_with_usage_path(Some(&context.project_dir))? else {
        return Err(missing_secret_key_message(env).into());
    };
    crate::keychain::require_export_authentication()?;
    let bundle = encode_key_bundle(key_id, &key);
    copy_to_clipboard(&bundle)?;

    output::success("Key copied to clipboard.");

    Ok(())
}

async fn import_key(
    context: &crate::commands::project_context::ProjectContext,
    passphrase: bool,
    requested_env: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = resolve_key_import_source(passphrase)?;
    match source {
        KeyImportSource::ExportedKey => import_exported_key(context, requested_env).await,
        KeyImportSource::Passphrase => import_passphrase_key(context, requested_env).await,
    }
}

async fn import_exported_key(
    context: &crate::commands::project_context::ProjectContext,
    requested_env: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = read_key_bundle()?;
    let (key_id, key) = decode_key_bundle(&input)?;
    let mut secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let mut secrets_changed = false;
    let env = if let Some(env) = requested_env {
        crate::config::validate_environment_name(env)?;
        if let Some(existing_key_id) = secrets.get_key_id(env) {
            if existing_key_id != key_id {
                return Err(format!("Exported key does not match {env} key.").into());
            }
        } else {
            secrets.set_env_key_id(env, &key_id)?;
            secrets_changed = true;
        }
        Some(env.to_string())
    } else {
        environment_for_key_id(&secrets, &key_id)
    };
    if let Some(env) = &env {
        validate_imported_key_for_env("Exported key", &secrets, env, &key)?;
    }

    let key_store = crate::crypto::KeyStore::for_key_id(&key_id)?;
    save_key_with_storage_prompt(&key_store, &key, env.as_deref(), Some(&context.project_dir))?;
    if secrets_changed {
        secrets.save_to_dir(&context.project_dir)?;
    }

    if let Some(env) = env {
        output::success(&format!("Imported {} key.", output::strong(&env)));
    } else {
        output::success(&format!("Imported key {}.", output::strong(&key_id)));
    }

    Ok(())
}

async fn import_passphrase_key(
    context: &crate::commands::project_context::ProjectContext,
    requested_env: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if requested_env.is_none() && !output::is_interactive() {
        return Err("Missing required environment. Pass --env with --passphrase.".into());
    }

    let env = resolve_secret_environment(context, requested_env, "Key environment")?;
    let mut secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let key_id = secrets.ensure_env_key_id(&env)?;
    let key = read_passphrase_key_for_env(&secrets, &env, &key_id)?;

    let key_store = crate::crypto::KeyStore::for_key_id(&key_id)?;
    save_key_with_storage_prompt(&key_store, &key, Some(&env), Some(&context.project_dir))?;
    secrets.save_to_dir(&context.project_dir)?;

    output::success(&format!("Imported {} key.", output::strong(&env)));

    Ok(())
}

fn validate_imported_key_for_env(
    source: &str,
    secrets: &crate::config::SecretsStore,
    env: &str,
    key: &crate::crypto::EncryptionKey,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(encrypted_secrets) = secrets.get_env(env) {
        for encrypted_value in encrypted_secrets.values() {
            crate::crypto::decrypt(encrypted_value, key)
                .map_err(|_| format!("{source} does not decrypt {env} secrets."))?;
        }
    }

    Ok(())
}

fn validate_passphrase_key_for_env(
    secrets: &crate::config::SecretsStore,
    env: &str,
    key: &crate::crypto::EncryptionKey,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_imported_key_for_env("Passphrase", secrets, env, key)
        .map_err(|_| INVALID_PASSPHRASE_ERROR.into())
}

fn missing_secret_key_message(env: &str) -> String {
    format!(
        "Unable to decrypt {env} secrets. Run `tako secrets key import` to import an exported key or passphrase."
    )
}

fn key_store_for_env(
    env: &str,
    secrets: &crate::config::SecretsStore,
) -> Result<crate::crypto::KeyStore, Box<dyn std::error::Error>> {
    let key_id = secrets.get_key_id(env).ok_or_else(|| {
        format!(
            "No key_id found for environment '{}'. This shouldn't happen — file a bug.",
            env
        )
    })?;

    Ok(crate::crypto::KeyStore::for_key_id(key_id)?)
}

/// Load a local environment key.
pub fn load_secret_key(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<crate::crypto::EncryptionKey, Box<dyn std::error::Error>> {
    let key_store = key_store_for_env(env, secrets)?;
    if let Some(key) = key_store.load_key_optional_with_usage_path(usage_path)? {
        return Ok(key);
    }

    Err(missing_secret_key_message(env).into())
}

fn load_or_create_key_for_set(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<crate::crypto::EncryptionKey, Box<dyn std::error::Error>> {
    let key_store = key_store_for_env(env, secrets)?;
    if let Some(key) = key_store.load_key_optional_with_usage_path(usage_path)? {
        return Ok(key);
    }
    if secrets.get_env(env).is_some_and(|map| !map.is_empty()) {
        return Err(missing_secret_key_message(env).into());
    }

    let key = crate::crypto::EncryptionKey::generate()?;
    save_key_with_storage_prompt(&key_store, &key, Some(env), usage_path)?;

    Ok(key)
}

fn save_key_with_storage_prompt(
    key_store: &crate::crypto::KeyStore,
    key: &crate::crypto::EncryptionKey,
    env: Option<&str>,
    usage_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let choice = resolve_key_storage_choice(env)?;
    save_key_with_storage_choice(key_store, key, choice, usage_path)?;

    Ok(())
}

fn save_key_with_storage_choice(
    key_store: &crate::crypto::KeyStore,
    key: &crate::crypto::EncryptionKey,
    choice: KeyStorageChoice,
    usage_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(target_os = "macos"))]
    let _ = usage_path;

    match choice {
        KeyStorageChoice::LocalFile => key_store.save_key(key)?,
        #[cfg(target_os = "macos")]
        KeyStorageChoice::ICloudKeychain => {
            save_key_to_icloud_keychain(key_store, key, usage_path)?
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn save_key_to_icloud_keychain(
    key_store: &crate::crypto::KeyStore,
    key: &crate::crypto::EncryptionKey,
    usage_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let key_id = key_store
        .key_id()
        .ok_or("iCloud Keychain storage requires a key id.")?;
    crate::keychain::save_key(key_id, key, usage_path).map_err(Into::into)
}

fn resolve_key_storage_choice(
    env: Option<&str>,
) -> Result<KeyStorageChoice, Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        use std::io::IsTerminal;

        if !std::io::stdin().is_terminal() {
            return Ok(KeyStorageChoice::LocalFile);
        }

        let hint = keychain_storage_hint(env);
        let use_icloud =
            output::confirm_with_description("Use iCloud Keychain?", Some(&hint), false)?;
        if use_icloud {
            Ok(KeyStorageChoice::ICloudKeychain)
        } else {
            Ok(KeyStorageChoice::LocalFile)
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = env;
        Ok(KeyStorageChoice::LocalFile)
    }
}

#[cfg(target_os = "macos")]
fn keychain_storage_hint(env: Option<&str>) -> String {
    match env {
        Some(env) => format!("Syncs {env} key to your other Macs."),
        None => "Syncs this key to your other Macs.".to_string(),
    }
}

/// Ensure the encryption key for `env` is available before starting
/// long-running rendering (e.g. the deploy task tree), so missing-key errors are
/// shown against a clean terminal.
///
/// Returns immediately if the env has no encrypted secrets (nothing to decrypt).
pub fn ensure_secret_key_available(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let has_secrets = secrets.get_env(env).is_some_and(|map| !map.is_empty());
    if !has_secrets {
        return Ok(());
    }
    let _ = load_secret_key(env, secrets, usage_path)?;
    Ok(())
}

fn resolve_app_name(config_path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    crate::app::require_app_name_from_config_path(config_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()).into())
}

fn copy_to_clipboard(text: &str) -> Result<(), Box<dyn std::error::Error>> {
    if text.is_empty() {
        return Err("Cannot copy empty key".into());
    }

    #[cfg(target_os = "macos")]
    {
        copy_to_clipboard_command("pbcopy", &[], text)
    }

    #[cfg(target_os = "linux")]
    {
        for (cmd, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ] {
            if copy_to_clipboard_command(cmd, args, text).is_ok() {
                return Ok(());
            }
        }

        Err("Failed to copy key to clipboard (tried wl-copy, xclip, xsel).".into())
    }

    #[cfg(target_os = "windows")]
    {
        copy_to_clipboard_command("clip", &[], text)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
        return Err("Clipboard export is not supported on this platform".into());
    }
}

fn copy_to_clipboard_command(
    cmd: &str,
    args: &[&str],
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or("Failed to open clipboard process stdin")?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Clipboard command '{}' failed", cmd).into())
    }
}

async fn sync_to_server(
    app_name: &str,
    server: &crate::config::ServerEntry,
    secrets: &std::collections::HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::ssh::SshClient;

    let mut ssh = SshClient::connect_to(&server.host, server.port).await?;

    // Push secrets through the management protocol; no remote .env file writes.
    let update_cmd = build_update_secrets_command(app_name, secrets)?;
    let response = ssh.tako_command(&update_cmd).await?;
    if tako_response_has_error(&response) {
        return Err(format!("tako-server error (update-secrets): {response}").into());
    }

    ssh.disconnect().await?;

    Ok(())
}

fn build_update_secrets_command(
    app_name: &str,
    secrets: &std::collections::HashMap<String, String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    serde_json::to_string(&Command::UpdateSecrets {
        app: app_name.to_string(),
        secrets: secrets.clone(),
    })
    .map_err(|e| format!("Failed to serialize update-secrets command: {e}").into())
}

fn tako_response_has_error(response: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(response)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(|status| status.as_str())
                .map(|status| status == "error")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServerEntry, ServersToml, TakoToml};
    use std::collections::HashMap;
    use std::ffi::OsString;
    use tempfile::TempDir;

    fn with_temp_tako_home<T>(f: impl FnOnce() -> T) -> T {
        let _lock = crate::paths::test_tako_home_env_lock();
        let temp = TempDir::new().unwrap();
        let previous = std::env::var_os("TAKO_HOME");
        unsafe {
            std::env::set_var("TAKO_HOME", temp.path());
        }

        struct ResetEnv(Option<OsString>);
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
                    None => unsafe { std::env::remove_var("TAKO_HOME") },
                }
            }
        }
        let _reset = ResetEnv(previous);
        f()
    }

    #[test]
    fn ensure_secret_key_available_is_noop_when_env_has_no_secrets() {
        with_temp_tako_home(|| {
            let mut secrets = crate::config::SecretsStore::parse("{}").unwrap();
            secrets.ensure_env_key_id("production").unwrap();
            // Env has a key_id but no secrets: nothing to decrypt later, no prompt needed.
            ensure_secret_key_available("production", &secrets, None)
                .expect("no-op when env has no secrets");
        });
    }

    #[test]
    fn ensure_secret_key_available_is_noop_when_env_has_no_key_id() {
        with_temp_tako_home(|| {
            let secrets = crate::config::SecretsStore::parse("{}").unwrap();
            // Env doesn't exist at all; skip without error.
            ensure_secret_key_available("production", &secrets, None)
                .expect("no-op when env is not initialized");
        });
    }

    #[test]
    fn ensure_secret_key_available_is_noop_when_key_already_cached() {
        with_temp_tako_home(|| {
            let json = r#"{
                "production": {
                    "key_id": "0123456789abcdef",
                    "secrets": {"DATABASE_URL": "opaque-encrypted-blob"}
                }
            }"#;
            let secrets = crate::config::SecretsStore::parse(json).unwrap();
            let key_id_b64 = secrets.get_key_id("production").unwrap();
            let key = crate::crypto::EncryptionKey::generate().unwrap();
            let key_store = crate::crypto::KeyStore::for_key_id(key_id_b64).unwrap();
            key_store.save_key(&key).unwrap();

            // With the key already on disk, this must not prompt — if it did, the
            // test would either hang or fail because stdin isn't a tty.
            ensure_secret_key_available("production", &secrets, None)
                .expect("cached key should be used without prompting");
        });
    }

    #[test]
    fn ensure_secret_key_available_errors_when_existing_secrets_have_no_key() {
        with_temp_tako_home(|| {
            let json = r#"{
                "production": {
                    "key_id": "0123456789abcdef",
                    "secrets": {"DATABASE_URL": "opaque-encrypted-blob"}
                }
            }"#;
            let secrets = crate::config::SecretsStore::parse(json).unwrap();
            let err = ensure_secret_key_available("production", &secrets, None).unwrap_err();
            assert_eq!(
                err.to_string(),
                "Unable to decrypt production secrets. Run `tako secrets key import` to import an exported key or passphrase."
            );
        });
    }

    #[test]
    fn missing_secret_key_message_names_environment_and_import_command() {
        assert_eq!(
            missing_secret_key_message("production"),
            "Unable to decrypt production secrets. Run `tako secrets key import` to import an exported key or passphrase."
        );
    }

    #[test]
    fn validate_passphrase_key_for_env_uses_short_invalid_passphrase_error() {
        let correct_key = crate::crypto::EncryptionKey::generate().unwrap();
        let encrypted = crate::crypto::encrypt("postgres://localhost/db", &correct_key).unwrap();
        let secrets = crate::config::SecretsStore::parse(&format!(
            r#"{{
                "production": {{
                    "key_id": "0123456789abcdef",
                    "secrets": {{"DATABASE_URL": "{encrypted}"}}
                }}
            }}"#
        ))
        .unwrap();
        let wrong_key = crate::crypto::EncryptionKey::generate().unwrap();

        let err = validate_passphrase_key_for_env(&secrets, "production", &wrong_key).unwrap_err();

        assert_eq!(err.to_string(), "Invalid passphrase.");
    }

    #[test]
    fn load_or_create_key_for_set_creates_random_key_for_empty_env() {
        with_temp_tako_home(|| {
            let mut secrets = crate::config::SecretsStore::parse("{}").unwrap();
            let key_id = secrets.ensure_env_key_id("development").unwrap();

            let key =
                load_or_create_key_for_set("development", &secrets, Some(Path::new("/tmp/tako")))
                    .expect("empty env should get a new local key");
            let key_store = crate::crypto::KeyStore::for_key_id(&key_id).unwrap();
            assert!(key_store.key_exists());
            assert_eq!(key_store.load_key().unwrap().as_bytes(), key.as_bytes());
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn i_cloud_keychain_choice_errors_when_signed_app_is_unavailable() {
        with_temp_tako_home(|| {
            let key_store = crate::crypto::KeyStore::for_key_id("0123456789abcdef").unwrap();
            let key = crate::crypto::EncryptionKey::generate().unwrap();

            let err = save_key_with_storage_choice(
                &key_store,
                &key,
                KeyStorageChoice::ICloudKeychain,
                Some(Path::new("/tmp/tako")),
            )
            .unwrap_err();

            assert_eq!(err.to_string(), crate::keychain::unavailable_message());
            assert!(!key_store.key_path().exists());
        });
    }

    #[test]
    fn local_file_choice_saves_key_to_disk() {
        with_temp_tako_home(|| {
            let key_store = crate::crypto::KeyStore::for_key_id("0123456789abcdef").unwrap();
            let key = crate::crypto::EncryptionKey::generate().unwrap();

            save_key_with_storage_choice(&key_store, &key, KeyStorageChoice::LocalFile, None)
                .unwrap();

            assert_eq!(key_store.load_key().unwrap().as_bytes(), key.as_bytes());
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_storage_hint_names_known_environment() {
        assert_eq!(
            keychain_storage_hint(Some("development")),
            "Syncs development key to your other Macs."
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_storage_hint_handles_unknown_environment() {
        assert_eq!(
            keychain_storage_hint(None),
            "Syncs this key to your other Macs."
        );
    }

    #[test]
    fn resolve_key_import_source_uses_passphrase_flag() {
        assert_eq!(
            resolve_key_import_source(true).unwrap(),
            KeyImportSource::Passphrase
        );
    }

    #[test]
    fn resolve_key_import_source_defaults_to_exported_key_non_interactively() {
        assert_eq!(
            resolve_key_import_source(false).unwrap(),
            KeyImportSource::ExportedKey
        );
    }

    #[test]
    fn secret_environment_options_show_defaults_then_existing_then_new() {
        let tako_config = TakoToml::parse(
            r#"
[envs.staging]
route = "staging.example.com"
"#,
        )
        .unwrap();
        let secrets = crate::config::SecretsStore::parse(
            r#"{
                "preview": {
                    "key_id": "0123456789abcdef",
                    "secrets": {}
                }
            }"#,
        )
        .unwrap();

        let labels: Vec<String> = secret_environment_options(&tako_config, &secrets)
            .into_iter()
            .map(|(label, _)| label)
            .collect();

        assert_eq!(
            labels,
            vec![
                "development".to_string(),
                "production".to_string(),
                "preview".to_string(),
                "staging".to_string(),
                "New environment".to_string(),
            ]
        );
    }

    #[test]
    fn replace_existing_value_prompt_omits_secret_name() {
        assert_eq!(
            replace_existing_value_prompt(),
            "Value is already set. Replace it?"
        );
    }

    #[test]
    fn key_bundle_round_trips_key_id_and_key() {
        with_temp_tako_home(|| {
            let key = crate::crypto::EncryptionKey::generate().unwrap();
            let key_id = "0123456789abcdef";

            let encoded = encode_key_bundle(key_id, &key);

            let (decoded_id, decoded_key) = decode_key_bundle(&encoded).unwrap();
            assert_eq!(decoded_id, key_id);
            assert_eq!(decoded_key.as_bytes(), key.as_bytes());
        });
    }

    #[test]
    fn key_bundle_rejects_invalid_payload() {
        let err = match decode_key_bundle("not-a-tako-key") {
            Ok(_) => panic!("expected invalid payload to fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("Invalid key payload"));
    }

    #[test]
    fn key_bundle_rejects_unsupported_version() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "id": "0123456789abcdef",
            "key": crate::crypto::EncryptionKey::generate().unwrap().to_base64(),
        }))
        .unwrap();
        let encoded = BASE64_URL.encode(payload);

        let err = match decode_key_bundle(&encoded) {
            Ok(_) => panic!("expected unsupported version to fail"),
            Err(err) => err,
        };
        assert_eq!(err.to_string(), "Unsupported key version: 2");
    }

    #[test]
    fn environment_for_key_id_matches_environment_key_id() {
        let secrets = crate::config::SecretsStore::parse(
            r#"{
                "development": {
                    "key_id": "0123456789abcdef",
                    "secrets": {}
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            environment_for_key_id(&secrets, "0123456789abcdef").as_deref(),
            Some("development")
        );
        assert_eq!(environment_for_key_id(&secrets, "fedcba9876543210"), None);
    }

    #[test]
    fn resolve_secret_sync_server_names_uses_explicit_mapping() {
        let tako_config = TakoToml::parse(
            r#"
[envs.production]
route = "app.example.com"
servers = ["solo"]
"#,
        )
        .unwrap();
        let mut servers = ServersToml::default();
        servers.servers.insert(
            "solo".to_string(),
            ServerEntry {
                host: "127.0.0.1".to_string(),
                port: 22,
                description: None,
            },
        );

        let names = resolve_secret_sync_server_names("production", &tako_config, &servers)
            .expect("should resolve");
        assert_eq!(names, vec!["solo".to_string()]);
    }

    #[test]
    fn resolve_secret_sync_server_names_returns_empty_for_unmapped_non_production() {
        let tako_config = TakoToml::default();
        let mut servers = ServersToml::default();
        servers.servers.insert(
            "solo".to_string(),
            ServerEntry {
                host: "127.0.0.1".to_string(),
                port: 22,
                description: None,
            },
        );

        let names = resolve_secret_sync_server_names("staging", &tako_config, &servers)
            .expect("should work");
        assert!(names.is_empty());
    }

    #[test]
    fn build_update_secrets_command_uses_protocol_payload_not_env_file_writes() {
        let secrets = HashMap::from([("API_KEY".to_string(), "secret".to_string())]);
        let command = build_update_secrets_command("my-app", &secrets).expect("serialize command");
        let value: serde_json::Value =
            serde_json::from_str(&command).expect("parse serialized command");

        assert_eq!(
            value.get("command").and_then(|v| v.as_str()),
            Some("update_secrets")
        );
        assert_eq!(value.get("app").and_then(|v| v.as_str()), Some("my-app"));
        assert_eq!(
            value
                .get("secrets")
                .and_then(|v| v.get("API_KEY"))
                .and_then(|v| v.as_str()),
            Some("secret")
        );
        assert!(!command.contains(".env"));
    }

    #[test]
    fn tako_response_has_error_only_accepts_structured_status_errors() {
        let json_err = r#"{"status":"error","message":"nope"}"#;
        let json_ok = r#"{"status":"ok","data":{}}"#;
        let old_error_shape = r#"{"error":"old-shape"}"#;
        let plain_text = "all good";

        assert!(tako_response_has_error(json_err));
        assert!(!tako_response_has_error(json_ok));
        assert!(!tako_response_has_error(old_error_shape));
        assert!(!tako_response_has_error(plain_text));
    }
}
