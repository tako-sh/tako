use crate::management_http::ManagementClient;
use crate::output;
use tako_core::{Command, Response};

use super::load_secret_key;

pub(super) async fn list_secrets(
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
            // CodeQL[rust/cleartext-logging]: list output shows secret names only, never values.
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

pub(super) async fn sync_secrets(
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
        && crate::commands::server::prompt_to_add_server(
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
                let mut decrypt_error = None;
                for (name, encrypted_value) in encrypted_secrets {
                    match decrypt(&encrypted_value.value, &key) {
                        Ok(value) => {
                            decrypted.insert(name.clone(), value);
                        }
                        Err(e) => {
                            decrypt_error = Some((name.clone(), e));
                            break;
                        }
                    }
                }
                // Syncing a partial set would delete the failed secret on the
                // server and restart the app without it — fail this env instead.
                if let Some((name, e)) = decrypt_error {
                    output::error(&format!(
                        "Failed to decrypt {} — {} not synced: {}",
                        output::strong(&name),
                        output::strong(env_name),
                        e
                    ));
                    error_count += 1;
                    continue;
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

pub(super) fn resolve_secret_sync_server_names(
    env_name: &str,
    tako_config: &crate::config::TakoToml,
    servers: &crate::config::ServersToml,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut resolved =
        match crate::commands::helpers::resolve_servers_for_env(tako_config, servers, env_name) {
            Ok(r) => r,
            Err(_) => return Ok(Vec::new()),
        };
    resolved.sort();
    resolved.dedup();
    Ok(resolved)
}

fn resolve_app_name(config_path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    crate::app::require_app_name_from_config_path(config_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()).into())
}

async fn sync_to_server(
    app_name: &str,
    server: &crate::config::ServerEntry,
    secrets: &std::collections::HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = ManagementClient::new(&server.host).await?;
    match client
        .send(&Command::UpdateSecrets {
            app: app_name.to_string(),
            secrets: secrets.clone(),
        })
        .await?
    {
        Response::Ok { .. } => Ok(()),
        Response::Error { message } => {
            Err(format!("tako-server error (update-secrets): {message}").into())
        }
    }
}
