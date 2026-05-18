use std::path::{Path, PathBuf};

use crate::build::{BuildAdapter, BuildPreset, PresetGroup};
use crate::config::{BuildConfig, BuildStage};
use crate::output;

use super::super::cache::sanitize_cache_label;
use super::super::format::format_stage_label;

/// Resolve the stage list to execute for a target build.
///
/// Precedence (first non-empty wins):
///   1. config `[[build_stages]]`
///   2. config `[build]` (normalized to a single-element stage list)
///   3. preset-declared stages (not yet supported; placeholder for future)
///   4. runtime default (single stage using the runtime's package-manager build command)
pub(super) fn resolve_build_stages(
    build_config: &BuildConfig,
    config_stages: &[BuildStage],
    _preset: &BuildPreset,
    runtime_default: Option<&str>,
) -> Vec<BuildStage> {
    if !config_stages.is_empty() {
        return config_stages.to_vec();
    }

    if let Some(run) = build_config
        .run
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return vec![BuildStage {
            name: None,
            cwd: build_config.cwd.clone(),
            install: build_config.install.clone(),
            run: run.to_string(),
            exclude: Vec::new(),
        }];
    }

    if let Some(run) = runtime_default.map(str::trim).filter(|s| !s.is_empty()) {
        return vec![BuildStage {
            name: Some("default".to_string()),
            cwd: None,
            install: None,
            run: run.to_string(),
            exclude: Vec::new(),
        }];
    }

    Vec::new()
}

pub(super) const LOCAL_BUILD_CACHE_RELATIVE_DIR: &str = ".tako/tmp/build-caches";

#[derive(Clone, Copy)]
enum LocalBuildCacheScope {
    Workspace,
    App,
}

#[derive(Clone, Copy)]
struct LocalBuildCacheSpec {
    relative_path: &'static str,
    scope: LocalBuildCacheScope,
}

const JS_LOCAL_BUILD_CACHE_SPECS: &[LocalBuildCacheSpec] = &[
    LocalBuildCacheSpec {
        relative_path: ".turbo",
        scope: LocalBuildCacheScope::Workspace,
    },
    LocalBuildCacheSpec {
        relative_path: ".next/cache",
        scope: LocalBuildCacheScope::App,
    },
];

fn local_build_cache_specs(runtime_adapter: BuildAdapter) -> &'static [LocalBuildCacheSpec] {
    if runtime_adapter.preset_group() == PresetGroup::Js {
        JS_LOCAL_BUILD_CACHE_SPECS
    } else {
        &[]
    }
}

pub(super) fn local_build_cache_root(project_dir: &Path, cache_target_label: &str) -> PathBuf {
    project_dir
        .join(LOCAL_BUILD_CACHE_RELATIVE_DIR)
        .join(sanitize_cache_label(cache_target_label))
}

pub(super) fn restore_local_build_caches(
    cache_root: &Path,
    workspace_root: &Path,
    app_dir: &Path,
    runtime_adapter: BuildAdapter,
) -> Result<usize, String> {
    let mut restored = 0usize;
    for spec in local_build_cache_specs(runtime_adapter) {
        let source = cache_root.join(spec.relative_path);
        if !source.is_dir() {
            continue;
        }
        let destination = local_build_cache_destination(spec, workspace_root, app_dir);
        replace_directory_from_cache(&source, &destination)?;
        restored += 1;
    }
    Ok(restored)
}

pub(super) fn persist_local_build_caches(
    cache_root: &Path,
    workspace_root: &Path,
    app_dir: &Path,
    runtime_adapter: BuildAdapter,
) -> Result<usize, String> {
    let mut persisted = 0usize;
    for spec in local_build_cache_specs(runtime_adapter) {
        let source = local_build_cache_destination(spec, workspace_root, app_dir);
        let destination = cache_root.join(spec.relative_path);
        if !source.is_dir() {
            remove_path_if_exists(&destination)?;
            continue;
        }
        replace_directory_from_cache(&source, &destination)?;
        persisted += 1;
    }
    Ok(persisted)
}

fn local_build_cache_destination(
    spec: &LocalBuildCacheSpec,
    workspace_root: &Path,
    app_dir: &Path,
) -> PathBuf {
    match spec.scope {
        LocalBuildCacheScope::Workspace => workspace_root.join(spec.relative_path),
        LocalBuildCacheScope::App => app_dir.join(spec.relative_path),
    }
}

fn replace_directory_from_cache(source: &Path, destination: &Path) -> Result<(), String> {
    remove_path_if_exists(destination)?;
    std::fs::create_dir_all(destination)
        .map_err(|e| format!("Failed to create {}: {e}", destination.display()))?;
    copy_dir_contents(source, destination)
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Failed to stat {}: {error}", path.display())),
    };

    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)
            .map_err(|e| format!("Failed to remove {}: {e}", path.display()))?;
        return Ok(());
    }

    std::fs::remove_dir_all(path).map_err(|e| format!("Failed to remove {}: {e}", path.display()))
}

pub(super) fn run_local_build(
    workspace: &Path,
    original_source_root: &Path,
    original_app_dir: &Path,
    stages: &[BuildStage],
    extra_envs: &[(&str, &str)],
) -> Result<(), String> {
    if !workspace.is_dir() {
        return Err(format!(
            "App directory '{}' does not exist inside build workspace",
            workspace.display()
        ));
    }

    if stages.is_empty() {
        return Ok(());
    }

    let app_dir_value = workspace.to_string_lossy().to_string();

    let run_shell =
        |cwd: &Path, command: &str, phase: &str, stage_label: &str| -> Result<(), String> {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-lc", command])
                .current_dir(cwd)
                .env("TAKO_APP_DIR", &app_dir_value);
            for (key, value) in extra_envs {
                cmd.env(key, value);
            }
            let output = cmd
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| format!("Failed to run local {stage_label} {phase} command: {e}"))?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if stderr.is_empty() { stdout } else { stderr };
            Err(format!("{stage_label} {phase} command failed: {detail}"))
        };

    for (index, stage) in stages.iter().enumerate() {
        let stage_label = format_stage_label(index + 1, stage.name.as_deref());
        let stage_cwd = resolve_stage_working_dir_for_local_build(
            original_source_root,
            original_app_dir,
            workspace,
            stage.cwd.as_deref(),
            &stage_label,
        )?;
        if let Some(install) = stage
            .install
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let _t = output::timed(&format!("{stage_label} install: {install}"));
            run_shell(&stage_cwd, install, "install", &stage_label)?;
        }
        let run_command = stage.run.trim();
        if run_command.is_empty() {
            return Err(format!("{stage_label} run command is empty"));
        }
        let _t = output::timed(&format!("{stage_label}: {run_command}"));
        run_shell(&stage_cwd, run_command, "run", &stage_label)?;
        drop(_t);
    }

    Ok(())
}

pub(super) fn summarize_build_stages(custom_stages: &[BuildStage]) -> Vec<String> {
    let mut labels = Vec::new();
    for (stage_number, stage) in custom_stages.iter().enumerate() {
        let stage_number = stage_number + 1;
        labels.push(format_stage_label(stage_number, stage.name.as_deref()));
    }
    labels
}

fn resolve_stage_working_dir_for_local_build(
    original_source_root: &Path,
    original_app_dir: &Path,
    workspace: &Path,
    working_dir: Option<&str>,
    stage_label: &str,
) -> Result<PathBuf, String> {
    let Some(working_dir) = working_dir.map(str::trim).filter(|value| !value.is_empty()) else {
        let relative_app = original_app_dir
            .strip_prefix(original_source_root)
            .unwrap_or(Path::new(""));
        return Ok(workspace.join(relative_app));
    };

    let relative_app = original_app_dir
        .strip_prefix(original_source_root)
        .unwrap_or(Path::new(""));
    let full_relative = relative_app.join(working_dir);
    let mut depth: i32 = 0;
    for component in full_relative.components() {
        match component {
            std::path::Component::ParentDir => depth -= 1,
            std::path::Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            return Err(format!(
                "{stage_label} working directory '{working_dir}' must not escape the project root",
            ));
        }
    }

    let resolved = original_app_dir.join(working_dir);
    if !resolved.is_dir() {
        return Err(format!(
            "{stage_label} working directory '{working_dir}' not found",
        ));
    }

    let canonical = resolved
        .canonicalize()
        .map_err(|_| format!("{stage_label} working directory '{working_dir}' not found"))?;
    let canonical_root = original_source_root.canonicalize().map_err(|e| {
        format!(
            "Failed to resolve source root '{}': {e}",
            original_source_root.display()
        )
    })?;
    let relative = canonical.strip_prefix(&canonical_root).map_err(|_| {
        format!("{stage_label} working directory '{working_dir}' must stay under the project root")
    })?;
    Ok(workspace.join(relative))
}

pub(super) fn copy_dir_contents(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in
        std::fs::read_dir(src).map_err(|e| format!("Failed to read {}: {e}", src.display()))?
    {
        let entry =
            entry.map_err(|e| format!("Failed to read dir entry in {}: {e}", src.display()))?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| format!("Failed to inspect {}: {e}", path.display()))?;
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("Failed to create {}: {e}", target.display()))?;
            copy_dir_contents(&path, &target)?;
        } else if file_type.is_file() {
            std::fs::copy(&path, &target).map_err(|e| {
                format!(
                    "Failed to copy {} to {}: {e}",
                    path.display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}
