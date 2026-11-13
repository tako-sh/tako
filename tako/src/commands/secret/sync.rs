use crate::output;

pub(super) async fn list_secrets(
    context: &crate::commands::project_context::ProjectContext,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::SecretsStore;
    let secrets = SecretsStore::load_from_dir(&context.project_dir)?;

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

    if output::is_json() {
        let listed = all_names
            .iter()
            .map(|name| {
                let environments: Vec<&str> = all_envs
                    .iter()
                    .filter(|env| secrets.contains(env, name))
                    .map(|env| env.as_str())
                    .collect();
                serde_json::json!({
                    "name": name,
                    "environments": environments,
                })
            })
            .collect::<Vec<_>>();
        return output::json_result(serde_json::json!({
            "ok": true,
            "command": "secrets",
            "secrets": listed,
        }));
    }

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
