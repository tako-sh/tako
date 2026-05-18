use std::path::Path;

pub(super) const RUNTIME_VERSION_OUTPUT_FILE: &str = ".tako-runtime-version";

pub(super) fn save_runtime_version_to_manifest(
    workspace: &Path,
    runtime_version: &str,
) -> Result<(), String> {
    save_manifest_version_field(workspace, "runtime_version", runtime_version)?;
    let _ = std::fs::remove_file(workspace.join(RUNTIME_VERSION_OUTPUT_FILE));
    Ok(())
}

pub(super) fn save_package_manager_version_to_manifest(
    workspace: &Path,
    package_manager_version: &str,
) -> Result<(), String> {
    save_manifest_version_field(
        workspace,
        "package_manager_version",
        package_manager_version,
    )
}

fn save_manifest_version_field(workspace: &Path, field: &str, version: &str) -> Result<(), String> {
    let manifest_path = workspace.join("app.json");
    let content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Failed to read {}: {e}", manifest_path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {e}", manifest_path.display()))?;
    value[field] = serde_json::Value::String(version.to_string());
    let updated = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("Failed to serialize {}: {e}", manifest_path.display()))?;
    std::fs::write(&manifest_path, updated)
        .map_err(|e| format!("Failed to write {}: {e}", manifest_path.display()))?;
    Ok(())
}

pub(super) fn extract_semver_from_version_output(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    for word in line.split_whitespace() {
        let word = word.trim_start_matches(|c: char| !c.is_ascii_digit());
        if word.chars().next().is_some_and(|c| c.is_ascii_digit())
            && word.contains('.')
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
        {
            return Some(word.to_string());
        }
    }
    None
}

pub(super) fn resolve_runtime_version_from_workspace(
    workspace: &Path,
    runtime_tool: &str,
) -> Result<String, String> {
    resolve_runtime_version_from_workspace_impl(workspace, runtime_tool, true)
}

pub(super) fn resolve_runtime_version_from_workspace_quiet(
    workspace: &Path,
    runtime_tool: &str,
) -> Result<String, String> {
    resolve_runtime_version_from_workspace_impl(workspace, runtime_tool, false)
}

fn resolve_runtime_version_from_workspace_impl(
    workspace: &Path,
    runtime_tool: &str,
    emit_warning: bool,
) -> Result<String, String> {
    if !workspace.is_dir() {
        return Err(format!(
            "App directory '{}' does not exist inside build workspace",
            workspace.display()
        ));
    }

    #[cfg(test)]
    {
        let _ = (workspace, runtime_tool, emit_warning);
        Ok("latest".to_string())
    }

    #[cfg(not(test))]
    {
        let command = format!(
            "{} --version",
            crate::shell::shell_single_quote(runtime_tool)
        );
        let output = std::process::Command::new("sh")
            .args(["-lc", &command])
            .current_dir(workspace)
            .stdin(std::process::Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if let Some(version) = extract_semver_from_version_output(&stdout) {
                    return Ok(version);
                }
                if emit_warning {
                    crate::output::warning(&format!(
                        "Could not detect {runtime_tool} version. Pin it with runtime = \"{runtime_tool}@<version>\" in tako.toml"
                    ));
                }
                Ok("latest".to_string())
            }
            _ => {
                if emit_warning {
                    crate::output::warning(&format!(
                        "Could not detect {runtime_tool} version. Pin it with runtime = \"{runtime_tool}@<version>\" in tako.toml"
                    ));
                }
                Ok("latest".to_string())
            }
        }
    }
}
