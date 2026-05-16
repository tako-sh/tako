mod local_build;
mod packaging;
mod runtime_version;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::build::{BuildAdapter, BuildCache, BuildExecutor, BuildPreset, PresetGroup};
use crate::config::TakoToml;
use crate::output;

use super::BuildPhaseResult;
use super::cache::cleanup_local_artifact_cache;
use super::format::{
    format_entry_point_summary, format_runtime_summary, format_server_targets_summary,
    should_use_unified_js_target_process,
};
use super::manifest::{
    build_deploy_archive_manifest, decrypt_deploy_secrets, resolve_deploy_main,
    resolve_deploy_version_and_source_hash, resolve_git_commit_message,
};
use super::task_tree::{ArtifactBuildGroup, DeployTaskTreeController};

pub(super) const LOCAL_ARTIFACT_CACHE_KEEP_TARGET_ARTIFACTS: usize = 90;

use local_build::{
    copy_dir_contents, resolve_build_stages, run_local_build, summarize_build_stages,
};
use local_build::{local_build_cache_root, persist_local_build_caches, restore_local_build_caches};
use packaging::build_target_artifacts;

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_build_phase(
    project_dir: PathBuf,
    source_root: PathBuf,
    eff_app_dir: PathBuf,
    app_name: String,
    env: String,
    tako_config: TakoToml,
    secrets: crate::config::SecretsStore,
    preset_ref: String,
    runtime_adapter: BuildAdapter,
    server_targets: Vec<(String, crate::config::ServerTarget)>,
    build_groups: Vec<ArtifactBuildGroup>,
    task_tree: Option<DeployTaskTreeController>,
) -> Result<BuildPhaseResult, String> {
    let phase = if task_tree.is_none() {
        Some(output::PhaseSpinner::start("Building…"))
    } else {
        None
    };
    let build_phase_timer = output::timed("Build phase");

    let executor = BuildExecutor::new(&project_dir);
    let cache = BuildCache::new(project_dir.join(".tako/artifacts"));
    cache.init().map_err(|e| e.to_string())?;
    match cleanup_local_artifact_cache(
        cache.cache_dir(),
        LOCAL_ARTIFACT_CACHE_KEEP_TARGET_ARTIFACTS,
    ) {
        Ok(summary) if summary.total_removed() > 0 => {
            tracing::debug!(
                "Local artifact cache cleanup: removed {} old artifact(s), {} stale metadata file(s)",
                summary.removed_target_artifacts,
                summary.removed_target_metadata
            );
        }
        Ok(_) => {}
        Err(error) => {
            if task_tree.is_none() {
                output::warning(&format!("Local artifact cache cleanup skipped: {}", error));
            } else {
                tracing::warn!("Local artifact cache cleanup skipped: {}", error);
            }
        }
    }
    let (version, _source_hash) = resolve_deploy_version_and_source_hash(&executor, &source_root)
        .map_err(|e| e.to_string())?;
    let git_commit_message = resolve_git_commit_message(&source_root);
    let git_dirty = executor.is_git_dirty().ok();
    tracing::debug!("Version: {}", version);

    let (mut build_preset, resolved_preset) = {
        let _t = output::timed(&format!("Resolve preset ref {preset_ref}"));
        if task_tree.is_none() {
            output::with_spinner_async(
                "Resolving build preset",
                "Build preset resolved",
                crate::build::load_build_preset(&eff_app_dir, &preset_ref),
            )
            .await
            .map_err(|e| e.to_string())?
        } else {
            crate::build::load_build_preset(&eff_app_dir, &preset_ref)
                .await
                .map_err(|e| e.to_string())?
        }
    };
    tracing::debug!(
        "Resolved preset: {} (commit {})",
        resolved_preset.preset_ref,
        super::format::shorten_commit(&resolved_preset.commit)
    );

    let plugin_ctx = tako_runtime::PluginContext {
        project_dir: &eff_app_dir,
        package_manager: tako_config.package_manager.as_deref(),
    };
    crate::build::apply_adapter_base_runtime_defaults(
        &mut build_preset,
        runtime_adapter,
        Some(&plugin_ctx),
    )
    .map_err(|e| e.to_string())?;
    tracing::debug!(
        "Build preset: {} @ {}",
        resolved_preset.preset_ref,
        super::format::shorten_commit(&resolved_preset.commit)
    );
    tracing::debug!("{}", format_runtime_summary(&build_preset.name, None));
    let runtime_tool = runtime_adapter.id().to_string();

    let manifest_main = resolve_deploy_main(
        &eff_app_dir,
        runtime_adapter,
        &tako_config,
        build_preset.main.as_deref(),
    )?;
    tracing::debug!(
        "{}",
        format_entry_point_summary(&eff_app_dir.join(&manifest_main),)
    );

    let env_idle_timeout = tako_config.get_idle_timeout(&env);
    let app_dir = project_dir
        .strip_prefix(&source_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let runtime_proj_root =
        tako_runtime::find_runtime_project_root(runtime_adapter.id(), &project_dir);
    let install_dir = runtime_proj_root
        .strip_prefix(&source_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let deploy_pm = tako_config
        .package_manager
        .clone()
        .or_else(|| tako_runtime::read_package_manager_spec(&eff_app_dir))
        .or_else(|| {
            tako_runtime::detect_package_manager(&eff_app_dir).map(|pm| pm.id().to_string())
        })
        .or_else(|| {
            tako_runtime::detect_package_manager(&runtime_proj_root).map(|pm| pm.id().to_string())
        });

    let mut runtime_env_vars = HashMap::new();
    if runtime_adapter.preset_group() == PresetGroup::Js {
        runtime_env_vars.insert(
            "TAKO_APP_ROOT".to_string(),
            tako_config.js_app_root().to_string(),
        );
    }

    let manifest = build_deploy_archive_manifest(
        &app_name,
        &env,
        &version,
        runtime_adapter.id(),
        &manifest_main,
        env_idle_timeout,
        deploy_pm,
        git_commit_message.clone(),
        git_dirty,
        tako_config.get_merged_vars(&env),
        runtime_env_vars,
        secrets.get_env(&env),
        tako_config.images.clone(),
        app_dir,
        install_dir,
    );
    let deploy_secrets =
        decrypt_deploy_secrets(&env, &secrets, Some(&project_dir)).map_err(|e| e.to_string())?;
    let deploy_storages = crate::commands::storage::decrypt_storage_bindings(
        &env,
        &tako_config,
        &secrets,
        Some(&project_dir),
    )
    .map_err(|e| e.to_string())?;

    let app_json_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;

    let runtime_default_build =
        tako_runtime::runtime_def_for(runtime_adapter.id(), Some(&plugin_ctx))
            .and_then(|def| def.preset.build);
    let resolved_stages = resolve_build_stages(
        &tako_config.build,
        &tako_config.build_stages,
        &build_preset,
        runtime_default_build.as_deref(),
    );

    let include_patterns = build_artifact_include_patterns(&tako_config);
    let exclude_patterns = build_artifact_exclude_patterns(&build_preset, &tako_config);
    let asset_roots = build_asset_roots(&build_preset, &tako_config)?;

    if let Some(server_targets_summary) = format_server_targets_summary(
        &server_targets,
        should_use_unified_js_target_process(&runtime_tool),
    ) {
        tracing::debug!("{}", server_targets_summary);
    }

    let artifacts_by_target = build_target_artifacts(
        &project_dir,
        &source_root,
        cache.cache_dir(),
        &app_json_bytes,
        &version,
        &runtime_tool,
        &build_groups,
        &resolved_stages,
        &include_patterns,
        &exclude_patterns,
        &asset_roots,
        tako_config.runtime_version.as_deref(),
        manifest.package_manager.as_deref(),
        task_tree.clone(),
    )
    .await?;

    drop(build_phase_timer);
    if let Some(phase) = phase {
        phase.finish("Built");
    }

    Ok(BuildPhaseResult {
        version,
        manifest_main,
        deploy_secrets,
        deploy_storages,
        use_unified_target_process: should_use_unified_js_target_process(&runtime_tool),
        artifacts_by_target,
    })
}

pub(super) fn build_artifact_include_patterns(config: &TakoToml) -> Vec<String> {
    // Stage `include` patterns are additive — they specify build outputs to
    // keep alongside the app's own files, not an exclusive filter.  The workdir
    // is already gitignore-filtered, so we include everything.
    if !config.build_stages.is_empty() {
        return vec!["**/*".to_string()];
    }
    if !config.build.include.is_empty() {
        return config.build.include.clone();
    }
    vec!["**/*".to_string()]
}

#[cfg(test)]
pub(super) fn should_report_artifact_include_patterns(include_patterns: &[String]) -> bool {
    if include_patterns.is_empty() {
        return false;
    }
    !(include_patterns.len() == 1 && include_patterns[0] == "**/*")
}

pub(super) fn build_artifact_exclude_patterns(
    _preset: &BuildPreset,
    config: &TakoToml,
) -> Vec<String> {
    if !config.build_stages.is_empty() {
        let mut patterns = Vec::new();
        for stage in &config.build_stages {
            for exclude in &stage.exclude {
                match &stage.cwd {
                    Some(cwd) if !cwd.is_empty() && cwd != "." => {
                        patterns.push(format!("{}/{}", cwd.trim_end_matches('/'), exclude));
                    }
                    _ => {
                        patterns.push(exclude.clone());
                    }
                }
            }
        }
        return patterns;
    }
    config.build.exclude.clone()
}

pub(super) fn build_asset_roots(
    preset: &BuildPreset,
    config: &TakoToml,
) -> Result<Vec<String>, String> {
    let mut merged = Vec::new();
    for root in preset.assets.iter().chain(config.assets.iter()) {
        let normalized = normalize_asset_root(root)?;
        if !merged.contains(&normalized) {
            merged.push(normalized);
        }
    }
    Ok(merged)
}

pub(super) fn normalize_asset_root(asset_root: &str) -> Result<String, String> {
    let trimmed = asset_root.trim();
    if trimmed.is_empty() {
        return Err("Configured assets entry cannot be empty".to_string());
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(format!(
            "Configured assets entry '{}' must be relative to project root",
            asset_root
        ));
    }

    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Configured assets entry '{}' must not contain '..'",
            asset_root
        ));
    }

    Ok(trimmed.replace('\\', "/"))
}
#[cfg(test)]
mod tests;
