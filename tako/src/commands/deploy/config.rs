use std::path::Path;

use crate::build::{
    BuildAdapter, infer_adapter_from_preset_reference, qualify_runtime_local_preset_ref,
};
use crate::commands::server;
use crate::config::{ServerTarget, ServersToml, TakoToml};
use crate::output;

use super::format::{
    format_bun_lockfile_preflight_error, format_environment_not_found_error,
    format_no_global_servers_error, format_no_servers_for_env_error, format_server_mapping_option,
    format_server_target_metadata_error,
};

pub(super) fn resolve_deploy_environment(
    requested_env: Option<&str>,
    tako_config: &TakoToml,
) -> Result<String, String> {
    let env = if let Some(env) = requested_env {
        if env == "development" {
            return Err(
                "Environment 'development' is reserved for local development and cannot be deployed."
                    .to_string(),
            );
        }
        env.to_string()
    } else {
        "production".to_string()
    };

    if !tako_config.envs.contains_key(env.as_str()) {
        let available: Vec<String> = tako_config.envs.keys().cloned().collect();
        return Err(format_environment_not_found_error(&env, &available));
    }

    Ok(env)
}

pub(super) fn required_env_routes(
    tako_config: &TakoToml,
    env: &str,
) -> Result<Vec<String>, String> {
    let routes = tako_config
        .get_routes(env)
        .ok_or_else(|| format!("Environment '{env}' has no routes configured"))?;
    if routes.is_empty() {
        return Err(format!(
            "Environment '{}' must define at least one route",
            env
        ));
    }
    Ok(routes)
}

pub(super) fn should_confirm_production_deploy(
    env: &str,
    assume_yes: bool,
    interactive: bool,
) -> bool {
    env == "production" && !assume_yes && interactive
}

pub(super) fn format_production_deploy_confirm_prompt() -> String {
    format!("Deploy to {} now?", output::strong("production"),)
}

pub(super) fn format_production_deploy_confirm_hint() -> String {
    output::theme_muted("Pass --yes/-y to skip this prompt.")
}

pub(super) fn confirm_production_deploy(env: &str, assume_yes: bool) -> std::io::Result<()> {
    if !should_confirm_production_deploy(env, assume_yes, output::is_interactive()) {
        return Ok(());
    }

    output::warning(&format!(
        "You are deploying to {}.",
        output::strong("production")
    ));
    let hint = format_production_deploy_confirm_hint();
    let confirmed = output::confirm_with_description(
        &format_production_deploy_confirm_prompt(),
        Some(&hint),
        false,
    )
    .map_err(|e| std::io::Error::new(e.kind(), format!("Failed to read confirmation: {e}")))?;
    if confirmed {
        Ok(())
    } else {
        Err(output::operation_cancelled_error())
    }
}

pub(super) fn resolve_deploy_server_names(
    tako_config: &TakoToml,
    servers: &ServersToml,
    env: &str,
) -> Result<Vec<String>, String> {
    let mut names = super::super::helpers::resolve_servers_for_env(tako_config, servers, env)?;
    names.sort();
    names.dedup();
    super::super::helpers::validate_server_names(&names, servers)?;
    Ok(names)
}

pub(super) async fn resolve_deploy_server_names_with_setup(
    tako_config: &TakoToml,
    servers: &mut ServersToml,
    env: &str,
    config_path: &Path,
) -> Result<Vec<String>, String> {
    match resolve_deploy_server_names(tako_config, servers, env) {
        Ok(names) => Ok(names),
        Err(original_error) => {
            if env != "production" {
                return Err(original_error);
            }

            if servers.is_empty() {
                let added = server::prompt_to_add_server(
                    "No servers have been added. Deployment needs at least one production server.",
                )
                .await
                .map_err(|e| format!("Failed to run server setup: {}", e))?;

                if added.is_none() {
                    return Err(original_error);
                }

                *servers = ServersToml::load().map_err(|e| e.to_string())?;
            }

            if servers.is_empty() {
                return Err(format_no_global_servers_error());
            }

            let selected_server = if servers.len() == 1 {
                servers.names()[0].to_string()
            } else {
                select_production_server_for_mapping(servers)?
            };

            persist_server_env_mapping(config_path, &selected_server, env)?;
            output::info(&format!(
                "Mapped server {} to {} in tako.toml",
                output::strong(&selected_server),
                output::strong(env)
            ));
            Ok(vec![selected_server])
        }
    }
}

pub(super) fn select_production_server_for_mapping(
    servers: &ServersToml,
) -> Result<String, String> {
    if !output::is_interactive() {
        return Err(format_no_servers_for_env_error("production"));
    }

    let mut names: Vec<&str> = servers.names();
    names.sort_unstable();

    let options = names
        .into_iter()
        .filter_map(|name| {
            servers
                .get(name)
                .map(|entry| (format_server_mapping_option(name, entry), name.to_string()))
        })
        .collect::<Vec<_>>();

    output::select(
        "Select server for production deploy",
        Some("No servers are configured for production. We will save your selection to tako.toml."),
        options,
    )
    .map_err(|e| format!("Failed to read selection: {}", e))
}

pub(super) fn persist_server_env_mapping(
    config_path: &Path,
    server_name: &str,
    env: &str,
) -> Result<(), String> {
    TakoToml::upsert_server_env_in_file(config_path, server_name, env).map_err(|e| {
        format!(
            "Failed to update tako.toml with [envs.{env}].servers including '{}': {}",
            server_name, e
        )
    })
}

pub(super) fn resolve_deploy_server_targets(
    servers: &ServersToml,
    server_names: &[String],
) -> Result<Vec<(String, ServerTarget)>, String> {
    let mut resolved = Vec::with_capacity(server_names.len());
    let mut missing = Vec::new();
    let mut invalid = Vec::new();

    for server_name in server_names {
        let Some(raw_target) = servers.get_target(server_name) else {
            missing.push(server_name.clone());
            continue;
        };

        match ServerTarget::normalized(&raw_target.arch, &raw_target.libc) {
            Ok(target) => resolved.push((server_name.clone(), target)),
            Err(err) => invalid.push(format!(
                "{} (arch='{}', libc='{}': {})",
                server_name, raw_target.arch, raw_target.libc, err
            )),
        }
    }

    if !missing.is_empty() || !invalid.is_empty() {
        return Err(format_server_target_metadata_error(&missing, &invalid));
    }

    Ok(resolved)
}

mod runtime_state;
pub(super) use runtime_state::validate_runtime_state_storage_for_deploy;

pub(super) fn resolve_build_adapter(
    project_dir: &Path,
    tako_config: &TakoToml,
) -> Result<BuildAdapter, String> {
    if let Some(adapter_override) = tako_config
        .runtime
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return BuildAdapter::from_id(adapter_override).ok_or_else(|| {
            format!(
                "Invalid runtime '{}'; expected one of: bun, node, go, rust",
                adapter_override
            )
        });
    }

    Ok(crate::build::detect_build_adapter(project_dir))
}

pub(super) fn resolve_effective_build_adapter(
    project_dir: &Path,
    tako_config: &TakoToml,
    preset_ref: &str,
) -> Result<BuildAdapter, String> {
    let configured_or_detected = resolve_build_adapter(project_dir, tako_config)?;
    if configured_or_detected != BuildAdapter::Unknown {
        return Ok(configured_or_detected);
    }

    let inferred = infer_adapter_from_preset_reference(preset_ref);
    if inferred != BuildAdapter::Unknown {
        return Ok(inferred);
    }

    Ok(configured_or_detected)
}

pub(super) fn resolve_build_preset_ref(
    project_dir: &Path,
    tako_config: &TakoToml,
) -> Result<String, String> {
    let runtime = resolve_build_adapter(project_dir, tako_config)?;
    if let Some(preset_ref) = tako_config
        .preset
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return qualify_runtime_local_preset_ref(runtime, preset_ref);
    }
    Ok(runtime.default_preset().to_string())
}

pub(super) fn should_run_bun_lockfile_preflight(runtime_adapter: BuildAdapter) -> bool {
    runtime_adapter == BuildAdapter::Bun
}

pub(super) fn has_bun_lockfile(workspace_root: &Path) -> bool {
    workspace_root.join("bun.lock").is_file() || workspace_root.join("bun.lockb").is_file()
}

pub(super) fn run_bun_lockfile_preflight(workspace_root: &Path) -> Result<bool, String> {
    if !has_bun_lockfile(workspace_root) {
        return Ok(false);
    }

    let output = std::process::Command::new("sh")
        .args(["-lc", "bun install --frozen-lockfile --lockfile-only"])
        .current_dir(workspace_root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("Failed to run Bun lockfile check: {e}"))?;
    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    Err(format_bun_lockfile_preflight_error(&detail))
}

#[cfg(test)]
mod tests;
