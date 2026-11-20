use crate::output;

/// Unlike `set` and `rm`, which only edit drafts, `clear` publishes at once:
/// it is meant for environments that may never be deployed or synced again.
pub(super) async fn clear_secrets(
    context: &crate::commands::project_context::ProjectContext,
    env: &str,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::config::validate_environment_name(env)?;
    if crate::commands::env_network::resolve_if_known(
        &context.config_path,
        env,
        crate::commands::env_network::Purpose::Use,
    )?
    .is_none()
    {
        return Err(format!(
            "Environment '{env}' has no network yet, so it has no published secrets to clear."
        )
        .into());
    }
    let app = super::sync::resolve_app_name(&context.config_path)?;

    if output::is_dry_run() {
        // Preview from this device's cache and drafts without contacting the network.
        let names = crate::network::secrets::preview_clear(&context.project_dir, &app, env)?;
        output::dry_run_skip(&format!("Clear {} from {env}", secret_count(names.len())));
        return finish(env, &names, true);
    }

    if !assume_yes {
        if !output::is_interactive() {
            return Err("Clearing secrets requires --yes in non-interactive mode.".into());
        }
        // Refresh the cache first so the confirmation lists the network's
        // current values rather than a stale preview.
        let names = tokio::task::block_in_place(|| {
            crate::network::secrets::sync_environment(
                &context.project_dir,
                &app,
                env,
                false,
                false,
            )?;
            crate::network::secrets::preview_clear(&context.project_dir, &app, env)
        })?;
        if names.is_empty() {
            output::info(&format!("No secrets to clear in {}", output::strong(env)));
            return finish(env, &names, false);
        }
        // CodeQL[rust/cleartext-logging]: the prompt lists secret names only, never values.
        let description = format!(
            "Deletes {} from the network now. Other devices drop them on their next sync.",
            names.join(", ")
        );
        let confirmed = output::confirm_with_description(
            &format!(
                "Clear {} from {}?",
                secret_count(names.len()),
                output::strong(env)
            ),
            Some(&description),
            false,
        )?;
        if !confirmed {
            return Err(output::operation_cancelled_error().into());
        }
    }

    let cleared = tokio::task::block_in_place(|| {
        crate::network::secrets::clear_environment(&context.project_dir, &app, env)
    })?;
    super::refresh_generated_files_after_secret_change(
        &context.project_dir,
        &context.config_path,
        Some(env),
    );
    if cleared.is_empty() {
        output::info(&format!("No secrets to clear in {}", output::strong(env)));
    } else {
        output::success(&format!(
            "Cleared {} from {}",
            secret_count(cleared.len()),
            output::strong(env)
        ));
        output::hint("Running apps keep their values until their next deployment.");
    }
    finish(env, &cleared, false)
}

fn secret_count(count: usize) -> String {
    if count == 1 {
        "1 secret".to_string()
    } else {
        format!("{count} secrets")
    }
}

fn finish(env: &str, names: &[String], dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !output::is_json() {
        return Ok(());
    }
    output::json_result(serde_json::json!({
        "ok": true,
        "command": "secrets",
        "environment": env,
        "cleared": names,
        "dry_run": dry_run,
    }))
}

#[cfg(test)]
mod tests {
    use super::secret_count;

    #[test]
    fn counts_use_singular_for_one_secret() {
        assert_eq!(secret_count(0), "0 secrets");
        assert_eq!(secret_count(1), "1 secret");
        assert_eq!(secret_count(3), "3 secrets");
    }
}
