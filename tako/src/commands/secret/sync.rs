use std::collections::BTreeMap;

use crate::network::secrets::LastChange;
use crate::output;

pub(super) async fn list_secrets(
    context: &crate::commands::project_context::ProjectContext,
) -> Result<(), Box<dyn std::error::Error>> {
    let secrets = context.load_secrets()?;

    if secrets.is_empty() {
        if output::is_json() {
            return output::json_result(serde_json::json!({
                "ok": true,
                "command": "secrets",
                "secrets": [],
            }));
        }
        output::warning("No secrets configured.");
        output::muted(&format!(
            "Run {} to add a secret.",
            output::strong("tako secrets set")
        ));
        return Ok(());
    }

    let all_names = secrets.all_secret_names();
    let all_envs = secrets.environment_names();

    let discrepancies = secrets.find_discrepancies();
    let changes = last_changes(context, &all_envs)?;
    let change = |env: &str, name: &str| changes.get(env).and_then(|changes| changes.get(name));

    if output::is_json() {
        let listed = all_names
            .iter()
            .map(|name| {
                let environments: Vec<&str> = all_envs
                    .iter()
                    .filter(|env| secrets.contains(env, name))
                    .map(|env| env.as_str())
                    .collect();
                let last_changed: serde_json::Map<String, serde_json::Value> = environments
                    .iter()
                    .filter_map(|env| {
                        let value = match change(env, name)? {
                            LastChange::Draft => serde_json::json!({"published": false}),
                            LastChange::Published { device, name } => serde_json::json!({
                                "published": true,
                                "device": name,
                                "device_id": device.to_string(),
                            }),
                        };
                        Some(((*env).to_owned(), value))
                    })
                    .collect();
                serde_json::json!({
                    "name": name,
                    "environments": environments,
                    "last_changed": last_changed,
                })
            })
            .collect::<Vec<_>>();
        return output::json_result(serde_json::json!({
            "ok": true,
            "command": "secrets",
            "secrets": listed,
        }));
    }

    let cell = |env: &str, name: &str| -> String {
        if !secrets.contains(env, name) {
            return "-".into();
        }
        match change(env, name) {
            Some(LastChange::Draft) => "[draft]".into(),
            Some(LastChange::Published { device, name }) => {
                format!("[set] by {}", device_label(device, name.as_deref()))
            }
            None => "[set]".into(),
        }
    };

    if output::is_pretty() {
        let widths: Vec<usize> = all_envs
            .iter()
            .map(|env| {
                all_names
                    .iter()
                    .map(|name| cell(env, name).chars().count())
                    .chain([env.chars().count(), 15])
                    .max()
                    .unwrap_or(15)
            })
            .collect();
        // Print header
        eprint!("{:<30}", "SECRET");
        for (env, width) in all_envs.iter().zip(&widths) {
            eprint!(" {:<width$}", env.to_uppercase());
        }
        eprintln!();

        eprint!("{}", "-".repeat(30));
        for width in &widths {
            eprint!(" {}", "-".repeat(*width));
        }
        eprintln!();

        // Print each secret
        for name in &all_names {
            // CodeQL[rust/cleartext-logging]: list output shows secret names and who changed them, never values.
            eprint!("{:<30}", name);
            for (env, width) in all_envs.iter().zip(&widths) {
                eprint!(" {:<width$}", cell(env, name));
            }

            eprintln!();
        }
    } else {
        for name in &all_names {
            let envs_with_secret: Vec<String> = all_envs
                .iter()
                .filter(|env| secrets.contains(env, name))
                .map(|env| match change(env, name) {
                    Some(LastChange::Draft) => format!("{env} (draft)"),
                    Some(LastChange::Published { device, name }) => {
                        format!("{env} (by {})", device_label(device, name.as_deref()))
                    }
                    None => env.clone(),
                })
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

/// Who last changed each secret, by environment and then secret name. Local
/// state only: listing never contacts a server.
fn last_changes(
    context: &crate::commands::project_context::ProjectContext,
    envs: &[String],
) -> Result<BTreeMap<String, BTreeMap<String, LastChange>>, Box<dyn std::error::Error>> {
    let config = crate::config::TakoToml::load_from_file(&context.config_path)?;
    let mut changes = BTreeMap::new();
    for env in envs {
        match crate::network::secrets::last_changes(&context.project_dir, &config, env) {
            Ok(env_changes) => {
                changes.insert(env.clone(), env_changes);
            }
            Err(error) => output::warning(&format!(
                "Could not read who last changed the {env} secrets: {error}"
            )),
        }
    }
    Ok(changes)
}

/// A device's name, or the start of its ID once it has left the network.
fn device_label(device: &tako_network::DeviceId, name: Option<&str>) -> String {
    match name {
        Some(name) => name.to_owned(),
        None => device.to_string().chars().take(12).collect(),
    }
}

pub(super) async fn sync_secrets(
    context: &crate::commands::project_context::ProjectContext,
    target_env: Option<&str>,
    retry_expired: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = crate::config::TakoToml::load_from_file(&context.config_path)?;
    let app = resolve_app_name(&context.config_path)?;
    let environments = match target_env {
        Some(env) if config.envs.contains_key(env) => vec![env.to_owned()],
        Some(_) => return Err("Environment is not configured in tako.toml".into()),
        None => config
            .envs
            .iter()
            .filter(|(_, spec)| spec.network.is_some())
            .map(|(env, _)| env.clone())
            .collect(),
    };
    if environments.is_empty() {
        return Err("No environment is bound to a network in tako.toml".into());
    }
    for env in environments {
        if output::is_dry_run() {
            output::dry_run_skip(&format!("Publish secret drafts for {env}"));
            continue;
        }
        tokio::task::block_in_place(|| {
            crate::network::secrets::sync_environment(
                &context.project_dir,
                &config,
                &app,
                &env,
                true,
                retry_expired,
            )
        })?;
        output::success(&format!("Published secret drafts for {env}"));
    }
    output::hint("Published values are applied to applications on their next deployment.");
    Ok(())
}

fn resolve_app_name(config_path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    crate::app::require_app_name_from_config_path(config_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()).into())
}
