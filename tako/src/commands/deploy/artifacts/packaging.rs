use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::build::{BuildAdapter, PresetGroup};
use crate::config::BuildStage;
use crate::output;

use super::super::cache::{
    ArtifactCachePaths, artifact_cache_paths, artifact_cache_temp_path, load_valid_cached_artifact,
    persist_cached_artifact, remove_cached_artifact_files,
};
use super::super::format::{
    format_artifact_cache_hit_message_for_output, format_artifact_cache_invalid_message,
    format_artifact_ready_message, format_artifact_ready_message_for_output,
    format_build_artifact_message, format_build_artifact_success, format_build_completed_message,
    format_build_stages_summary_for_output, format_path_relative_to,
    format_prepare_artifact_message, format_prepare_artifact_success, format_runtime_probe_message,
    format_runtime_probe_success, format_size, should_use_local_build_spinners,
};
use super::super::task_tree::{ArtifactBuildGroup, DeployTaskTreeController};
use super::runtime_version::{
    resolve_runtime_version_from_workspace, resolve_runtime_version_from_workspace_quiet,
    save_package_manager_version_to_manifest, save_runtime_version_to_manifest,
};
use super::{
    copy_dir_contents, local_build_cache_root, persist_local_build_caches,
    restore_local_build_caches, run_local_build, summarize_build_stages,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn build_target_artifacts(
    project_dir: &Path,
    source_root: &Path,
    cache_dir: &Path,
    app_manifest_bytes: &[u8],
    version: &str,
    runtime_tool: &str,
    target_groups: &[ArtifactBuildGroup],
    resolved_stages: &[BuildStage],
    include_patterns: &[String],
    exclude_patterns: &[String],
    asset_roots: &[String],
    pinned_runtime_version: Option<&str>,
    package_manager_tool: Option<&str>,
    task_tree: Option<DeployTaskTreeController>,
) -> Result<HashMap<String, PathBuf>, String> {
    let has_multiple_targets = target_groups.len() > 1;
    let mut artifacts = HashMap::new();
    let runtime_adapter = BuildAdapter::from_id(runtime_tool).unwrap_or(BuildAdapter::Unknown);
    let runtime_version_tool = runtime_adapter.version_probe_tool();

    for target_group in target_groups.iter().cloned() {
        let build_target_label = target_group.build_target_label;
        let cache_target_label = target_group.cache_target_label;
        let build_cache_root = local_build_cache_root(project_dir, &cache_target_label);
        let display_target_label = target_group.display_target_label.as_deref();
        let tree_target_label = display_target_label.unwrap_or("shared target").to_string();
        let use_local_build_spinners =
            task_tree.is_none() && should_use_local_build_spinners(output::is_interactive());
        let stage_summary = summarize_build_stages(resolved_stages);
        if let Some(stage_summary_message) =
            format_build_stages_summary_for_output(&stage_summary, display_target_label)
        {
            tracing::debug!("{}", stage_summary_message);
        }

        let build_dir = project_dir.join(".tako/build");
        crate::build::cleanup_workdir(&build_dir);
        {
            let _t = output::timed("Build dir setup");
            crate::build::create_workdir(source_root, &build_dir)
                .map_err(|e| format!("Failed to create build dir: {e}"))?;
            if runtime_adapter.preset_group() == PresetGroup::Js {
                crate::build::symlink_node_modules(source_root, &build_dir)
                    .map_err(|e| format!("Failed to symlink node_modules: {e}"))?;
            }
        }
        let workspace = build_dir.clone();
        let app_dir_in_workspace = match project_dir.strip_prefix(source_root) {
            Ok(rel) if !rel.as_os_str().is_empty() => workspace.join(rel),
            _ => workspace.clone(),
        };

        match restore_local_build_caches(
            &build_cache_root,
            &workspace,
            &app_dir_in_workspace,
            runtime_adapter,
        ) {
            Ok(restored) if restored > 0 => {
                tracing::debug!(
                    "Restored {} local build cache director{} for {}",
                    restored,
                    if restored == 1 { "y" } else { "ies" },
                    cache_target_label
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    "Failed to restore local build caches for {}: {}",
                    cache_target_label,
                    error
                );
            }
        }

        std::fs::write(workspace.join("app.json"), app_manifest_bytes)
            .map_err(|e| format!("Failed to write app.json: {e}"))?;

        let runtime_probe_label = format_runtime_probe_message(display_target_label);
        let runtime_probe_success = format_runtime_probe_success(display_target_label);
        let runtime_version = if let Some(pinned) = pinned_runtime_version {
            tracing::debug!("Using pinned runtime version {} from tako.toml", pinned);
            if let Some(task_tree) = &task_tree {
                task_tree.skip_build_step(
                    &tree_target_label,
                    "probe-runtime",
                    format!("Pinned: {pinned}"),
                );
            }
            pinned.to_string()
        } else if task_tree.is_some() {
            if let Some(task_tree) = &task_tree {
                task_tree.mark_build_step_running(&tree_target_label, "probe-runtime");
            }
            let version_result =
                resolve_runtime_version_from_workspace_quiet(&workspace, runtime_version_tool);
            match version_result {
                Ok(version) => {
                    if let Some(task_tree) = &task_tree {
                        task_tree.succeed_build_step(
                            &tree_target_label,
                            "probe-runtime",
                            Some(version.clone()),
                        );
                    }
                    version
                }
                Err(error) => {
                    if let Some(task_tree) = &task_tree {
                        task_tree.fail_build_step(
                            &tree_target_label,
                            "probe-runtime",
                            error.clone(),
                        );
                        task_tree.fail_build_target(&tree_target_label, error.clone());
                        task_tree.cancel_pending_build_children(&tree_target_label, "cancelled");
                    }
                    return Err(error);
                }
            }
        } else if use_local_build_spinners {
            output::with_spinner(&runtime_probe_label, &runtime_probe_success, || {
                let _t = output::timed(&format!(
                    "Probe {} version in {}",
                    runtime_version_tool,
                    workspace.display()
                ));
                let version =
                    resolve_runtime_version_from_workspace(&workspace, runtime_version_tool);
                if let Ok(v) = &version {
                    tracing::debug!("Detected {} {}", runtime_version_tool, v);
                }
                version
            })?
        } else {
            tracing::debug!("{}", runtime_probe_label);
            let _t = output::timed(&format!("Probe {} version", runtime_version_tool));
            let version = resolve_runtime_version_from_workspace(&workspace, runtime_version_tool)?;
            drop(_t);
            tracing::debug!("Detected {} {}", runtime_version_tool, version);
            version
        };
        let package_manager_version = package_manager_tool.map(|tool| {
            if tool == runtime_tool {
                return runtime_version.clone();
            }
            resolve_runtime_version_from_workspace_quiet(&workspace, tool)
                .unwrap_or_else(|_| "latest".to_string())
        });

        let target_label_for_path = if has_multiple_targets {
            Some(cache_target_label.as_str())
        } else {
            None
        };
        let cache_paths = artifact_cache_paths(cache_dir, version, target_label_for_path);

        match load_valid_cached_artifact(&cache_paths) {
            Ok(Some(cached)) => {
                tracing::debug!(
                    "Artifact cache hit: {} ({})",
                    format_path_relative_to(project_dir, &cached.path),
                    format_size(cached.size_bytes)
                );
                if let Some(task_tree) = &task_tree {
                    task_tree.skip_build_step(&tree_target_label, "build-artifact", "skipped");
                    task_tree.skip_build_step(&tree_target_label, "package-artifact", "skipped");
                    task_tree.append_cached_artifact_step(
                        &tree_target_label,
                        Some(format_size(cached.size_bytes)),
                    );
                    task_tree.succeed_build_target(
                        &tree_target_label,
                        Some(format!("{} (cached)", format_size(cached.size_bytes))),
                    );
                } else if has_multiple_targets {
                    output::bullet(&format_build_completed_message(display_target_label));
                } else {
                    output::bullet(&format_artifact_cache_hit_message_for_output(
                        display_target_label,
                    ));
                }
                for target_label in &target_group.target_labels {
                    artifacts.insert(target_label.clone(), cached.path.clone());
                }
                continue;
            }
            Ok(None) => {
                tracing::debug!("Artifact cache miss, building from source");
            }
            Err(error) => {
                if task_tree.is_none() {
                    output::warning(&format_artifact_cache_invalid_message(
                        display_target_label,
                        &error,
                    ));
                } else {
                    tracing::warn!(
                        "{}",
                        format_artifact_cache_invalid_message(display_target_label, &error)
                    );
                }
                remove_cached_artifact_files(&cache_paths);
            }
        }

        let go_cross_envs: Vec<(&str, String)> = if runtime_adapter.preset_group()
            == PresetGroup::Go
        {
            let goarch =
                if build_target_label.contains("aarch64") || build_target_label.contains("arm64") {
                    "arm64"
                } else {
                    "amd64"
                };
            vec![
                ("GOOS", "linux".to_string()),
                ("GOARCH", goarch.to_string()),
            ]
        } else {
            Vec::new()
        };
        let extra_envs: Vec<(&str, &str)> = go_cross_envs
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();

        let build_result = (|| -> Result<u64, String> {
            let build_label = format_build_artifact_message(display_target_label);
            let build_success = format_build_artifact_success(display_target_label);
            if let Some(task_tree) = &task_tree {
                task_tree.mark_build_step_running(&tree_target_label, "build-artifact");
                if let Err(error) = run_local_build(
                    &workspace,
                    source_root,
                    project_dir,
                    resolved_stages,
                    &extra_envs,
                ) {
                    task_tree.fail_build_step(&tree_target_label, "build-artifact", error.clone());
                    task_tree.fail_build_target(&tree_target_label, error.clone());
                    task_tree.cancel_pending_build_children(&tree_target_label, "cancelled");
                    return Err(error);
                }
                task_tree.succeed_build_step(&tree_target_label, "build-artifact", None);
            } else if use_local_build_spinners {
                output::with_spinner(&build_label, &build_success, || {
                    let _t = output::timed(&format!("Target build ({build_target_label})"));
                    run_local_build(
                        &workspace,
                        source_root,
                        project_dir,
                        resolved_stages,
                        &extra_envs,
                    )
                })?;
            } else {
                output::bullet(&build_label);
                let _t = output::timed("Target build");
                run_local_build(
                    &workspace,
                    source_root,
                    project_dir,
                    resolved_stages,
                    &extra_envs,
                )?;
            }
            save_runtime_version_to_manifest(&workspace, &runtime_version)?;
            if let Some(version) = package_manager_version.as_deref() {
                save_package_manager_version_to_manifest(&workspace, version)?;
            }
            match persist_local_build_caches(
                &build_cache_root,
                &workspace,
                &app_dir_in_workspace,
                runtime_adapter,
            ) {
                Ok(persisted) if persisted > 0 => {
                    tracing::debug!(
                        "Saved {} local build cache director{} for {}",
                        persisted,
                        if persisted == 1 { "y" } else { "ies" },
                        cache_target_label
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        "Failed to persist local build caches for {}: {}",
                        cache_target_label,
                        error
                    );
                }
            }
            tracing::debug!("{}", format_build_completed_message(display_target_label));

            let prepare_label = format_prepare_artifact_message(display_target_label);
            let prepare_success = format_prepare_artifact_success(display_target_label);
            if let Some(task_tree) = &task_tree {
                task_tree.mark_build_step_running(&tree_target_label, "package-artifact");
                if let Err(error) = package_target_artifact(
                    &workspace,
                    &app_dir_in_workspace,
                    asset_roots,
                    include_patterns,
                    exclude_patterns,
                    &cache_paths,
                    &build_target_label,
                ) {
                    task_tree.fail_build_step(
                        &tree_target_label,
                        "package-artifact",
                        error.clone(),
                    );
                    task_tree.fail_build_target(&tree_target_label, error.clone());
                    return Err(error);
                }
                Ok(cache_paths
                    .artifact_path
                    .metadata()
                    .map_err(|e| format!("Failed to read artifact metadata: {e}"))?
                    .len())
            } else if use_local_build_spinners {
                output::with_spinner(&prepare_label, &prepare_success, || {
                    let _t = output::timed(&format!("Artifact packaging ({build_target_label})"));
                    package_target_artifact(
                        &workspace,
                        &app_dir_in_workspace,
                        asset_roots,
                        include_patterns,
                        exclude_patterns,
                        &cache_paths,
                        &build_target_label,
                    )
                })
            } else {
                output::bullet(&prepare_label);
                let _t = output::timed(&format!("Artifact packaging ({build_target_label})"));
                package_target_artifact(
                    &workspace,
                    &app_dir_in_workspace,
                    asset_roots,
                    include_patterns,
                    exclude_patterns,
                    &cache_paths,
                    &build_target_label,
                )
            }
        })();
        let artifact_size = build_result?;

        if let Some(task_tree) = &task_tree {
            task_tree.succeed_build_step(
                &tree_target_label,
                "package-artifact",
                Some(format_size(artifact_size)),
            );
            task_tree.succeed_build_target(&tree_target_label, Some(format_size(artifact_size)));
        }

        tracing::debug!(
            "{}",
            format_artifact_ready_message_for_output(display_target_label)
        );
        tracing::debug!(
            "{}",
            format_artifact_ready_message(
                display_target_label,
                &format_path_relative_to(project_dir, &cache_paths.artifact_path),
                &format_size(artifact_size),
            )
        );
        for target_label in &target_group.target_labels {
            artifacts.insert(target_label.clone(), cache_paths.artifact_path.clone());
        }
        if task_tree.is_none() && has_multiple_targets {
            output::bullet(&format_build_completed_message(display_target_label));
        }

        crate::build::cleanup_workdir(&build_dir);
    }

    Ok(artifacts)
}

pub(super) async fn build_container_target_artifacts(
    project_dir: &Path,
    source_root: &Path,
    cache_dir: &Path,
    app_manifest_bytes: &[u8],
    version: &str,
    target_groups: &[ArtifactBuildGroup],
    task_tree: Option<DeployTaskTreeController>,
) -> Result<HashMap<String, PathBuf>, String> {
    let mut artifacts = HashMap::new();

    for target_group in target_groups.iter().cloned() {
        let tree_target_label = target_group
            .display_target_label
            .as_deref()
            .unwrap_or("shared target")
            .to_string();
        let build_dir = project_dir.join(".tako/build");
        crate::build::cleanup_workdir(&build_dir);

        if let Some(task_tree) = &task_tree {
            task_tree.mark_build_step_running(&tree_target_label, "package-artifact");
        } else {
            output::bullet(&format_prepare_artifact_message(
                target_group.display_target_label.as_deref(),
            ));
        }

        let result = (|| -> Result<u64, String> {
            crate::build::create_workdir(source_root, &build_dir)
                .map_err(|e| format!("Failed to create container build context: {e}"))?;
            std::fs::write(build_dir.join("app.json"), app_manifest_bytes)
                .map_err(|e| format!("Failed to write app.json: {e}"))?;

            let target_label_for_path =
                (target_groups.len() > 1).then_some(target_group.cache_target_label.as_str());
            let cache_paths = artifact_cache_paths(cache_dir, version, target_label_for_path);
            package_target_artifact(
                &build_dir,
                &build_dir,
                &[],
                &["**/*".to_string()],
                &[],
                &cache_paths,
                &target_group.build_target_label,
            )?;
            cache_paths
                .artifact_path
                .metadata()
                .map_err(|e| format!("Failed to read artifact metadata: {e}"))
                .map(|metadata| metadata.len())
        })();

        crate::build::cleanup_workdir(&build_dir);
        let artifact_size = match result {
            Ok(size) => size,
            Err(error) => {
                if let Some(task_tree) = &task_tree {
                    task_tree.fail_build_step(
                        &tree_target_label,
                        "package-artifact",
                        error.clone(),
                    );
                    task_tree.fail_build_target(&tree_target_label, error.clone());
                }
                return Err(error);
            }
        };

        let target_label_for_path =
            (target_groups.len() > 1).then_some(target_group.cache_target_label.as_str());
        let cache_paths = artifact_cache_paths(cache_dir, version, target_label_for_path);
        for target_label in &target_group.target_labels {
            artifacts.insert(target_label.clone(), cache_paths.artifact_path.clone());
        }
        if let Some(task_tree) = &task_tree {
            task_tree.succeed_build_step(
                &tree_target_label,
                "package-artifact",
                Some(format_size(artifact_size)),
            );
            task_tree.succeed_build_target(&tree_target_label, Some(format_size(artifact_size)));
        }
        tracing::debug!(
            "{}",
            format_artifact_ready_message(
                target_group.display_target_label.as_deref(),
                &format_path_relative_to(project_dir, &cache_paths.artifact_path),
                &format_size(artifact_size),
            )
        );
    }

    Ok(artifacts)
}

pub(super) fn merge_assets_locally(
    workspace_root: &Path,
    asset_roots: &[String],
) -> Result<(), String> {
    if asset_roots.is_empty() {
        return Ok(());
    }

    if !workspace_root.is_dir() {
        return Err(format!(
            "App directory '{}' does not exist inside build workspace",
            workspace_root.display()
        ));
    }

    let public_dir = workspace_root.join("public");
    std::fs::create_dir_all(&public_dir)
        .map_err(|e| format!("Failed to create {}: {e}", public_dir.display()))?;

    for asset_root in asset_roots {
        if asset_root == "public" {
            continue;
        }
        let src = workspace_root.join(asset_root);
        if !src.is_dir() {
            return Err(format!(
                "Configured asset directory '{}' not found after build.",
                asset_root
            ));
        }
        copy_dir_contents(&src, &public_dir)?;
    }

    Ok(())
}

pub(super) fn package_target_artifact(
    workspace: &Path,
    app_dir: &Path,
    asset_roots: &[String],
    include_patterns: &[String],
    exclude_patterns: &[String],
    cache_paths: &ArtifactCachePaths,
    target_label: &str,
) -> Result<u64, String> {
    merge_assets_locally(app_dir, asset_roots)?;

    let artifact_temp_path = artifact_cache_temp_path(&cache_paths.artifact_path)?;
    let artifact_size = crate::build::create_workdir_archive(
        workspace,
        &artifact_temp_path,
        include_patterns,
        exclude_patterns,
    )
    .map_err(|e| format!("Failed to create artifact for {}: {}", target_label, e))?;
    tracing::debug!(
        "Artifact size for {}: {}",
        target_label,
        format_size(artifact_size)
    );

    if let Err(error) = persist_cached_artifact(&artifact_temp_path, cache_paths, artifact_size) {
        let _ = std::fs::remove_file(&artifact_temp_path);
        let _ = std::fs::remove_file(&cache_paths.metadata_path);
        return Err(format!(
            "Failed to persist cached artifact for {}: {}",
            target_label, error
        ));
    }

    Ok(artifact_size)
}
