use std::path::Path;

use crate::build::{BuildAdapter, detect_build_adapter};
use crate::config::{POSTGRES_CREDENTIAL_NAME, SecretsStore, TakoToml};
use crate::validation::{SECRET_EXPIRY_WARNING_DAYS, ValidationResult};

pub(in crate::commands::deploy) fn validate_runtime_state_storage_for_deploy(
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
    let adapter = tako_config
        .runtime
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(BuildAdapter::from_id)
        .unwrap_or_else(|| detect_build_adapter(project_dir));
    if adapter == BuildAdapter::Go && project_dir.join("cmd/worker/main.go").is_file() {
        return WorkflowStorageIntent::RequiresRemote;
    }

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
