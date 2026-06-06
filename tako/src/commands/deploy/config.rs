use std::path::Path;

use crate::build::{
    BuildAdapter, infer_adapter_from_preset_reference, qualify_runtime_local_preset_ref,
};
use crate::commands::server;
use crate::config::{POSTGRES_CREDENTIAL_NAME, SecretsStore, ServerTarget, ServersToml, TakoToml};
use crate::output;
use crate::validation::{SECRET_EXPIRY_WARNING_DAYS, ValidationResult};

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

pub(super) fn validate_runtime_state_storage_for_deploy(
    project_dir: &Path,
    tako_config: &TakoToml,
    secrets: &SecretsStore,
    env: &str,
    server_count: usize,
) -> ValidationResult {
    let mut result = ValidationResult::new();
    let workflow_storage = project_workflow_storage(project_dir, tako_config);
    let has_channels = project_has_channels(project_dir, tako_config);
    if workflow_storage == WorkflowStorageIntent::NoWorkflows && !has_channels {
        return result;
    }

    if server_count > 1 && workflow_storage == WorkflowStorageIntent::AllLocal && !has_channels {
        return result;
    }

    let postgres_credential = secrets.get_credential(env, POSTGRES_CREDENTIAL_NAME);
    if server_count > 1 && postgres_credential.is_none() {
        result.error(format!(
            "{} in environment '{env}' target {server_count} servers. {}",
            runtime_state_subject(workflow_storage, has_channels),
            missing_postgres_storage_action(workflow_storage, has_channels, env)
        ));
        return result;
    }

    let Some(credential) = postgres_credential else {
        return result;
    };

    match credential.is_expired() {
        Ok(true) => {
            if let Some(expires_on) = &credential.expires_on {
                result.error(format!(
                    "Credential {POSTGRES_CREDENTIAL_NAME} for environment '{env}' expired on {expires_on}. Run `tako credentials set {POSTGRES_CREDENTIAL_NAME} --env {env}` to update it."
                ));
            }
        }
        Ok(false) => match credential.is_expiring_within_days(SECRET_EXPIRY_WARNING_DAYS) {
            Ok(true) => {
                if let Some(expires_on) = &credential.expires_on {
                    result.warn(format!(
                        "Credential {POSTGRES_CREDENTIAL_NAME} for environment '{env}' expires within {SECRET_EXPIRY_WARNING_DAYS} days on {expires_on}. Run `tako credentials set {POSTGRES_CREDENTIAL_NAME} --env {env}` to rotate it."
                    ));
                }
            }
            Ok(false) => {}
            Err(error) => result.error(format!(
                "Credential {POSTGRES_CREDENTIAL_NAME} for environment '{env}' has invalid expiry metadata: {error}"
            )),
        },
        Err(error) => result.error(format!(
            "Credential {POSTGRES_CREDENTIAL_NAME} for environment '{env}' has invalid expiry metadata: {error}"
        )),
    }

    result
}

fn runtime_state_subject(
    workflow_storage: WorkflowStorageIntent,
    has_channels: bool,
) -> &'static str {
    match (
        workflow_storage != WorkflowStorageIntent::NoWorkflows,
        has_channels,
    ) {
        (true, true) => "Channels and workflows",
        (true, false) => "Workflows",
        (false, true) => "Channels",
        (false, false) => "Runtime state",
    }
}

fn missing_postgres_storage_action(
    workflow_storage: WorkflowStorageIntent,
    has_channels: bool,
    env: &str,
) -> String {
    match (workflow_storage, has_channels) {
        (_, true) => format!(
            "Run `tako credentials set {POSTGRES_CREDENTIAL_NAME} --env {env}` for shared channel/workflow storage."
        ),
        (WorkflowStorageIntent::RequiresRemote, false) => format!(
            "Mark every workflow with `local: true` for per-server local storage, or run `tako credentials set {POSTGRES_CREDENTIAL_NAME} --env {env}` for remote workflow storage."
        ),
        _ => format!(
            "Run `tako credentials set {POSTGRES_CREDENTIAL_NAME} --env {env}` for remote runtime state storage."
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowStorageIntent {
    NoWorkflows,
    AllLocal,
    RequiresRemote,
}

fn project_workflow_storage(project_dir: &Path, tako_config: &TakoToml) -> WorkflowStorageIntent {
    let workflows_dir =
        crate::build::js::js_app_root_dir(project_dir, tako_config.js_app_root()).join("workflows");
    let entries = match std::fs::read_dir(workflows_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return WorkflowStorageIntent::NoWorkflows;
        }
        Err(_) => return WorkflowStorageIntent::RequiresRemote,
    };

    let mut saw_workflow = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return WorkflowStorageIntent::RequiresRemote;
        };
        let path = entry.path();
        if !is_js_runtime_source_file(&path) {
            continue;
        }
        saw_workflow = true;
        let Ok(source) = std::fs::read_to_string(&path) else {
            return WorkflowStorageIntent::RequiresRemote;
        };
        if !source_declares_local_workflow(&source) {
            return WorkflowStorageIntent::RequiresRemote;
        }
    }

    if saw_workflow {
        WorkflowStorageIntent::AllLocal
    } else {
        WorkflowStorageIntent::NoWorkflows
    }
}

fn project_has_channels(project_dir: &Path, tako_config: &TakoToml) -> bool {
    let channels_dir =
        crate::build::js::js_app_root_dir(project_dir, tako_config.js_app_root()).join("channels");
    let entries = match std::fs::read_dir(channels_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };

    for entry in entries {
        let Ok(entry) = entry else {
            return true;
        };
        if is_js_runtime_source_file(&entry.path()) {
            return true;
        }
    }
    false
}

fn is_js_runtime_source_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if file_name.starts_with('.') || file_name.starts_with('_') {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("ts" | "tsx" | "js" | "mjs" | "mts")
    )
}

fn source_declares_local_workflow(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => i = skip_js_string_like(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = skip_js_line_comment(bytes, i + 2),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_js_block_comment(bytes, i + 2),
            b'd' if starts_with_token(bytes, i, b"defineWorkflow") => {
                if define_workflow_call_declares_local(bytes, i + "defineWorkflow".len()) {
                    return true;
                }
                i += "defineWorkflow".len();
            }
            _ => i += 1,
        }
    }
    false
}

fn define_workflow_call_declares_local(bytes: &[u8], mut i: usize) -> bool {
    i = skip_js_ws_and_comments(bytes, i);
    i = skip_ts_type_args(bytes, i);
    i = skip_js_ws_and_comments(bytes, i);
    if bytes.get(i) != Some(&b'(') {
        return false;
    }

    let Some(opts_start) = find_define_workflow_options_object(bytes, i) else {
        return false;
    };
    options_object_declares_local(bytes, opts_start)
}

fn find_define_workflow_options_object(bytes: &[u8], call_start: usize) -> Option<usize> {
    let mut i = call_start + 1;
    let mut paren_depth = 1usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => i = skip_js_string_like(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = skip_js_line_comment(bytes, i + 2),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_js_block_comment(bytes, i + 2),
            b'(' => {
                paren_depth += 1;
                i += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                if paren_depth == 0 {
                    return None;
                }
                i += 1;
            }
            b'{' => {
                brace_depth += 1;
                i += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                i += 1;
            }
            b'[' => {
                bracket_depth += 1;
                i += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                i += 1;
            }
            b',' if paren_depth == 1 && brace_depth == 0 && bracket_depth == 0 => {
                let next = skip_js_ws_and_comments(bytes, i + 1);
                return (bytes.get(next) == Some(&b'{')).then_some(next);
            }
            _ => i += 1,
        }
    }
    None
}

fn options_object_declares_local(bytes: &[u8], object_start: usize) -> bool {
    let mut i = object_start + 1;
    let mut brace_depth = 1usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => i = skip_js_string_like(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = skip_js_line_comment(bytes, i + 2),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_js_block_comment(bytes, i + 2),
            b'(' => {
                paren_depth += 1;
                i += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                i += 1;
            }
            b'{' => {
                brace_depth += 1;
                i += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                if brace_depth == 0 {
                    return false;
                }
                i += 1;
            }
            b'[' => {
                bracket_depth += 1;
                i += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                i += 1;
            }
            b'l' if brace_depth == 1
                && paren_depth == 0
                && bracket_depth == 0
                && starts_with_token(bytes, i, b"local") =>
            {
                let mut next = skip_js_ws_and_comments(bytes, i + 5);
                if bytes.get(next) == Some(&b':') {
                    next = skip_js_ws_and_comments(bytes, next + 1);
                    if starts_with_token(bytes, next, b"true") {
                        return true;
                    }
                }
                i += 5;
            }
            _ => i += 1,
        }
    }
    false
}

fn skip_ts_type_args(bytes: &[u8], mut i: usize) -> usize {
    if bytes.get(i) != Some(&b'<') {
        return i;
    }

    let mut depth = 1usize;
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => i = skip_js_string_like(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = skip_js_line_comment(bytes, i + 2),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_js_block_comment(bytes, i + 2),
            b'<' => {
                depth += 1;
                i += 1;
            }
            b'>' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

fn skip_js_ws_and_comments(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while bytes.get(i).is_some_and(|byte| byte.is_ascii_whitespace()) {
            i += 1;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'/') {
            i = skip_js_line_comment(bytes, i + 2);
            continue;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i = skip_js_block_comment(bytes, i + 2);
            continue;
        }
        return i;
    }
}

fn skip_js_line_comment(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_js_block_comment(bytes: &[u8], mut i: usize) -> usize {
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            return i + 2;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_js_string_like(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn is_token_boundary(bytes: &[u8], start: usize, len: usize) -> bool {
    !bytes
        .get(start.wrapping_sub(1))
        .is_some_and(|b| is_ident_byte(*b))
        && !bytes.get(start + len).is_some_and(|b| is_ident_byte(*b))
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn starts_with_token(bytes: &[u8], start: usize, token: &[u8]) -> bool {
    bytes
        .get(start..start.saturating_add(token.len()))
        .is_some_and(|candidate| candidate == token)
        && is_token_boundary(bytes, start, token.len())
}

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
                "Invalid runtime '{}'; expected one of: bun, node, go",
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
