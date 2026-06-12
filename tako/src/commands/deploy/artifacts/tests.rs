use super::super::cache::artifact_cache_paths;
use super::packaging::{merge_assets_locally, package_target_artifact};
use super::runtime_version::{
    RUNTIME_VERSION_OUTPUT_FILE, extract_semver_from_version_output,
    resolve_runtime_version_from_workspace, save_package_manager_version_to_manifest,
    save_runtime_version_to_manifest,
};
use super::*;
use crate::build::{BuildExecutor, BuildPreset};
use crate::commands::deploy::format::format_build_stages_summary_for_output;
use tempfile::TempDir;

#[test]
fn normalize_asset_root_rejects_invalid_paths() {
    assert!(normalize_asset_root(" ").is_err());
    assert!(normalize_asset_root("/tmp/assets").is_err());
    assert!(normalize_asset_root("../assets").is_err());
}

#[test]
fn build_asset_roots_combines_and_deduplicates_preset_and_project_values() {
    let preset = BuildPreset {
        name: "bun".to_string(),
        main: None,
        assets: vec!["public".to_string(), "dist/client".to_string()],
        dev: vec![],
        runtime_overrides: Default::default(),
    };
    let config = TakoToml {
        assets: vec!["dist/client".to_string(), "assets/shared".to_string()],
        ..Default::default()
    };
    let merged = build_asset_roots(&preset, &config).unwrap();
    assert_eq!(
        merged,
        vec![
            "public".to_string(),
            "dist/client".to_string(),
            "assets/shared".to_string()
        ]
    );
}

#[test]
fn build_artifact_include_patterns_uses_project_values_when_set() {
    let config = TakoToml {
        build: crate::config::BuildConfig {
            include: vec!["custom/**".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };
    let includes = build_artifact_include_patterns(&config);
    assert_eq!(includes, vec!["custom/**".to_string()]);
}

#[test]
fn build_artifact_include_patterns_defaults_to_all_when_unset() {
    let includes = build_artifact_include_patterns(&TakoToml::default());
    assert_eq!(includes, vec!["**/*".to_string()]);
}

#[test]
fn build_artifact_include_patterns_stages_include_everything() {
    let mut config = TakoToml::default();
    config.build_stages = vec![
        crate::config::BuildStage {
            name: Some("rust".to_string()),
            cwd: Some("rust-service".to_string()),
            install: None,
            run: "cargo build --release".to_string(),
            exclude: Vec::new(),
        },
        crate::config::BuildStage {
            name: Some("frontend".to_string()),
            cwd: Some("apps/web".to_string()),
            install: None,
            run: "bun run build".to_string(),
            exclude: vec!["**/*.map".to_string()],
        },
    ];
    let includes = build_artifact_include_patterns(&config);
    assert_eq!(includes, vec!["**/*".to_string()]);
}

#[test]
fn build_artifact_exclude_patterns_collects_from_stages() {
    let mut config = TakoToml::default();
    config.build_stages = vec![
        crate::config::BuildStage {
            name: None,
            cwd: Some("apps/web".to_string()),
            install: None,
            run: "bun run build".to_string(),
            exclude: vec!["**/*.map".to_string()],
        },
        crate::config::BuildStage {
            name: None,
            cwd: None,
            install: None,
            run: "bun run build".to_string(),
            exclude: vec!["tmp/**".to_string()],
        },
    ];
    let excludes = build_artifact_exclude_patterns(&BuildPreset::default(), &config);
    assert_eq!(
        excludes,
        vec!["apps/web/**/*.map".to_string(), "tmp/**".to_string()]
    );
}

#[test]
fn should_report_artifact_include_patterns_hides_default_wildcard() {
    assert!(!should_report_artifact_include_patterns(&[
        "**/*".to_string()
    ]));
}

#[test]
fn should_report_artifact_include_patterns_shows_custom_patterns() {
    assert!(should_report_artifact_include_patterns(&[
        "dist/**".to_string()
    ]));
    assert!(should_report_artifact_include_patterns(&[
        "dist/**".to_string(),
        ".output/**".to_string()
    ]));
}

#[test]
fn resolve_go_workflow_worker_main_detects_conventional_worker() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("cmd/worker")).unwrap();
    std::fs::write(temp.path().join("cmd/worker/main.go"), "package main").unwrap();

    assert_eq!(
        resolve_workflow_worker_main(temp.path(), crate::build::BuildAdapter::Go).as_deref(),
        Some("worker")
    );
}

#[test]
fn resolve_go_workflow_worker_main_ignores_projects_without_worker() {
    let temp = TempDir::new().unwrap();

    assert_eq!(
        resolve_workflow_worker_main(temp.path(), crate::build::BuildAdapter::Go),
        None
    );
}

#[test]
fn resolve_build_stages_prefers_config_stages() {
    let build = crate::config::BuildConfig {
        run: Some("should-not-run".to_string()),
        ..Default::default()
    };
    let stages = vec![crate::config::BuildStage {
        name: Some("app".to_string()),
        cwd: None,
        install: None,
        run: "bun run build".to_string(),
        exclude: Vec::new(),
    }];
    let resolved = resolve_build_stages(&build, &stages, &BuildPreset::default(), Some("fallback"));
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name.as_deref(), Some("app"));
    assert_eq!(resolved[0].run, "bun run build");
}

#[test]
fn resolve_build_stages_uses_build_run_when_stages_empty() {
    let build = crate::config::BuildConfig {
        run: Some("make build".to_string()),
        install: Some("make deps".to_string()),
        cwd: Some("server".to_string()),
        ..Default::default()
    };
    let resolved = resolve_build_stages(&build, &[], &BuildPreset::default(), Some("fallback"));
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].run, "make build");
    assert_eq!(resolved[0].install.as_deref(), Some("make deps"));
    assert_eq!(resolved[0].cwd.as_deref(), Some("server"));
}

#[test]
fn resolve_build_stages_falls_back_to_runtime_default() {
    let resolved = resolve_build_stages(
        &crate::config::BuildConfig::default(),
        &[],
        &BuildPreset::default(),
        Some("bun run --if-present build"),
    );
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name.as_deref(), Some("default"));
    assert_eq!(resolved[0].run, "bun run --if-present build");
}

#[test]
fn resolve_build_stages_returns_empty_when_nothing_configured() {
    let resolved = resolve_build_stages(
        &crate::config::BuildConfig::default(),
        &[],
        &BuildPreset::default(),
        None,
    );
    assert!(resolved.is_empty());
}

#[test]
fn summarize_build_stages_lists_custom_stages() {
    let custom = vec![
        crate::config::BuildStage {
            name: None,
            cwd: None,
            install: None,
            run: "bun run build".to_string(),
            exclude: Vec::new(),
        },
        crate::config::BuildStage {
            name: Some("frontend-assets".to_string()),
            cwd: Some("frontend".to_string()),
            install: None,
            run: "bun run build".to_string(),
            exclude: Vec::new(),
        },
    ];
    assert_eq!(
        summarize_build_stages(&custom),
        vec!["Stage 1".to_string(), "Stage 'frontend-assets'".to_string()]
    );
}

#[test]
fn run_local_build_executes_custom_stages_in_order() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("frontend")).unwrap();
    let stages = vec![
        crate::config::BuildStage {
            name: None,
            cwd: None,
            install: None,
            run: "printf 'stage-1-run\\n' >> \"$TAKO_APP_DIR/order.log\"".to_string(),
            exclude: Vec::new(),
        },
        crate::config::BuildStage {
            name: Some("frontend-assets".to_string()),
            cwd: Some("frontend".to_string()),
            install: Some("printf 'stage-2-install\\n' >> \"$TAKO_APP_DIR/order.log\"".to_string()),
            run: "printf 'stage-2-run\\n' >> \"$TAKO_APP_DIR/order.log\"".to_string(),
            exclude: Vec::new(),
        },
    ];

    run_local_build(&workspace, &workspace, &workspace, &stages, &[]).unwrap();
    let order = std::fs::read_to_string(workspace.join("order.log")).unwrap();
    assert_eq!(order, "stage-1-run\nstage-2-install\nstage-2-run\n");
}

#[test]
fn run_local_build_errors_when_stage_working_dir_is_missing() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let stages = vec![crate::config::BuildStage {
        name: None,
        cwd: Some("frontend".to_string()),
        install: None,
        run: "true".to_string(),
        exclude: Vec::new(),
    }];

    let err = run_local_build(&workspace, &workspace, &workspace, &stages, &[]).unwrap_err();
    assert!(err.contains("Stage 1"));
    assert!(err.contains("working directory"));
}

#[test]
fn run_local_build_defaults_cwd_to_app_dir() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let app_dir = workspace.join("apps/myapp");
    std::fs::create_dir_all(&app_dir).unwrap();
    let stages = vec![crate::config::BuildStage {
        name: None,
        cwd: None,
        install: None,
        run: "touch marker.txt".to_string(),
        exclude: Vec::new(),
    }];
    run_local_build(&workspace, &workspace, &app_dir, &stages, &[]).unwrap();
    assert!(app_dir.join("marker.txt").exists());
    assert!(!workspace.join("marker.txt").exists());
}

#[test]
fn run_local_build_stage_cwd_relative_to_app_dir() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let app_dir = workspace.join("apps/myapp");
    let sdk_dir = workspace.join("sdk");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&sdk_dir).unwrap();
    let stages = vec![crate::config::BuildStage {
        name: Some("sdk".to_string()),
        cwd: Some("../../sdk".to_string()),
        install: None,
        run: "touch built.txt".to_string(),
        exclude: Vec::new(),
    }];
    run_local_build(&workspace, &workspace, &app_dir, &stages, &[]).unwrap();
    assert!(sdk_dir.join("built.txt").exists());
}

#[test]
fn run_local_build_stage_cwd_rejects_workspace_escape() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let app_dir = workspace.join("apps/myapp");
    std::fs::create_dir_all(&app_dir).unwrap();
    let stages = vec![crate::config::BuildStage {
        name: None,
        cwd: Some("../../../outside".to_string()),
        install: None,
        run: "true".to_string(),
        exclude: Vec::new(),
    }];
    let err = run_local_build(&workspace, &workspace, &app_dir, &stages, &[]).unwrap_err();
    assert!(err.contains("must not escape the project root"));
}

#[test]
#[cfg(unix)]
fn run_local_build_stage_cwd_rejects_symlink_escape() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let app_dir = workspace.join("apps/myapp");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, app_dir.join("linked-outside")).unwrap();
    let stages = vec![crate::config::BuildStage {
        name: None,
        cwd: Some("linked-outside".to_string()),
        install: None,
        run: "true".to_string(),
        exclude: Vec::new(),
    }];

    let err = run_local_build(&workspace, &workspace, &app_dir, &stages, &[]).unwrap_err();
    assert!(err.contains("must stay under the project root"));
}

#[test]
fn merge_assets_locally_merges_into_public_and_overwrites_last_write() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("dist/client")).unwrap();
    std::fs::create_dir_all(workspace.join("assets/shared")).unwrap();
    std::fs::write(workspace.join("dist/client/logo.txt"), "dist").unwrap();
    std::fs::write(workspace.join("assets/shared/logo.txt"), "shared").unwrap();

    merge_assets_locally(
        &workspace,
        &["dist/client".to_string(), "assets/shared".to_string()],
    )
    .unwrap();

    let merged = std::fs::read_to_string(workspace.join("public/logo.txt")).unwrap();
    assert_eq!(merged, "shared");
}

#[test]
fn merge_assets_locally_fails_when_asset_root_is_missing() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let err = merge_assets_locally(&workspace, &["missing".to_string()]).unwrap_err();
    assert!(err.contains("not found after build"));
}

#[test]
fn restore_local_build_caches_copies_workspace_and_app_scoped_directories() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let workspace = temp.path().join("workspace");
    let app_dir = workspace.join("apps/web");

    std::fs::create_dir_all(cache_root.join(".turbo")).unwrap();
    std::fs::create_dir_all(cache_root.join(".next/cache")).unwrap();
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(cache_root.join(".turbo/state.json"), "workspace-cache").unwrap();
    std::fs::write(cache_root.join(".next/cache/fetch-cache"), "app-cache").unwrap();

    let restored =
        restore_local_build_caches(&cache_root, &workspace, &app_dir, BuildAdapter::Node).unwrap();

    assert_eq!(restored, 2);
    assert_eq!(
        std::fs::read_to_string(workspace.join(".turbo/state.json")).unwrap(),
        "workspace-cache"
    );
    assert_eq!(
        std::fs::read_to_string(app_dir.join(".next/cache/fetch-cache")).unwrap(),
        "app-cache"
    );
}

#[test]
fn persist_local_build_caches_overwrites_stale_entries() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let workspace = temp.path().join("workspace");
    let app_dir = workspace.join("apps/web");

    std::fs::create_dir_all(cache_root.join(".turbo")).unwrap();
    std::fs::create_dir_all(cache_root.join(".next/cache")).unwrap();
    std::fs::create_dir_all(workspace.join(".turbo")).unwrap();
    std::fs::create_dir_all(app_dir.join(".next/cache")).unwrap();
    std::fs::write(cache_root.join(".turbo/stale.txt"), "stale").unwrap();
    std::fs::write(cache_root.join(".next/cache/stale.txt"), "stale").unwrap();
    std::fs::write(workspace.join(".turbo/state.json"), "fresh-workspace").unwrap();
    std::fs::write(app_dir.join(".next/cache/fetch-cache"), "fresh-app").unwrap();

    let persisted =
        persist_local_build_caches(&cache_root, &workspace, &app_dir, BuildAdapter::Node).unwrap();

    assert_eq!(persisted, 2);
    assert_eq!(
        std::fs::read_to_string(cache_root.join(".turbo/state.json")).unwrap(),
        "fresh-workspace"
    );
    assert_eq!(
        std::fs::read_to_string(cache_root.join(".next/cache/fetch-cache")).unwrap(),
        "fresh-app"
    );
    assert!(!cache_root.join(".turbo/stale.txt").exists());
    assert!(!cache_root.join(".next/cache/stale.txt").exists());
}

#[test]
fn persist_local_build_caches_removes_entries_when_build_did_not_recreate_them() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let workspace = temp.path().join("workspace");
    let app_dir = workspace.join("apps/web");

    std::fs::create_dir_all(cache_root.join(".turbo")).unwrap();
    std::fs::create_dir_all(cache_root.join(".next/cache")).unwrap();
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(cache_root.join(".turbo/stale.txt"), "stale").unwrap();
    std::fs::write(cache_root.join(".next/cache/stale.txt"), "stale").unwrap();

    let persisted =
        persist_local_build_caches(&cache_root, &workspace, &app_dir, BuildAdapter::Node).unwrap();

    assert_eq!(persisted, 0);
    assert!(!cache_root.join(".turbo").exists());
    assert!(!cache_root.join(".next/cache").exists());
}

#[test]
fn local_build_cache_root_sanitizes_target_labels() {
    let temp = TempDir::new().unwrap();
    let root = local_build_cache_root(temp.path(), "linux/arm64 (shared)");
    assert!(root.ends_with(".tako/tmp/build-caches/linux_arm64__shared_"));
}

#[test]
fn save_runtime_version_to_manifest_writes_version_to_app_json() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("app.json"),
        r#"{"runtime":"bun","main":"index.ts","idle_timeout":300}"#,
    )
    .unwrap();

    save_runtime_version_to_manifest(&workspace, "1.3.9").unwrap();

    let manifest_raw = std::fs::read_to_string(workspace.join("app.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
    assert_eq!(manifest["runtime_version"], "1.3.9");
    assert_eq!(manifest["runtime"], "bun");
}

#[test]
fn save_package_manager_version_to_manifest_writes_version_to_app_json() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("app.json"),
        r#"{"runtime":"node","main":"index.ts","idle_timeout":300,"package_manager":"bun"}"#,
    )
    .unwrap();

    save_package_manager_version_to_manifest(&workspace, "1.3.11").unwrap();

    let manifest_raw = std::fs::read_to_string(workspace.join("app.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
    assert_eq!(manifest["package_manager_version"], "1.3.11");
    assert_eq!(manifest["package_manager"], "bun");
}

#[test]
fn extract_semver_from_version_output_handles_common_formats() {
    assert_eq!(
        extract_semver_from_version_output("bun 1.3.11"),
        Some("1.3.11".to_string())
    );
    assert_eq!(
        extract_semver_from_version_output("node v22.7.0"),
        Some("22.7.0".to_string())
    );
    assert_eq!(
        extract_semver_from_version_output("v22.12.0"),
        Some("22.12.0".to_string())
    );
}

#[test]
fn save_runtime_version_cleans_up_old_version_file() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("app.json"),
        r#"{"runtime":"bun","main":"index.ts","idle_timeout":300}"#,
    )
    .unwrap();
    std::fs::write(workspace.join(RUNTIME_VERSION_OUTPUT_FILE), "1.3.9").unwrap();

    save_runtime_version_to_manifest(&workspace, "1.3.9").unwrap();

    assert!(!workspace.join(RUNTIME_VERSION_OUTPUT_FILE).exists());
}

#[test]
fn resolve_runtime_version_from_workspace_ignores_old_runtime_version_file() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let old_tools_file = format!(".{}{}", "proto", "tools");
    std::fs::write(workspace.join(old_tools_file), "bun = \"1.3.9\"\n").unwrap();

    let resolved =
        resolve_runtime_version_from_workspace(&workspace, "bun").expect("resolve runtime version");

    assert_eq!(resolved, "latest");
}

#[test]
fn package_target_artifact_packages_workspace_root_contents() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("index.ts"), "console.log('ok');").unwrap();
    std::fs::write(workspace.join("app.json"), r#"{"main":"index.ts"}"#).unwrap();

    let cache_dir = temp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let cache_paths = artifact_cache_paths(&cache_dir, "v1", Some("linux-aarch64-musl"));
    let archive_size = package_target_artifact(
        &workspace,
        &workspace,
        &[],
        &["**/*".to_string()],
        &[],
        &cache_paths,
        "linux-aarch64-musl",
    )
    .unwrap();
    assert!(archive_size > 0);

    let unpacked = temp.path().join("unpacked");
    BuildExecutor::extract_archive(&cache_paths.artifact_path, &unpacked).unwrap();

    assert!(unpacked.join("index.ts").exists());
    assert!(unpacked.join("app.json").exists());
}

#[test]
fn package_target_artifact_for_bun_does_not_require_entrypoint_sources() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("index.ts"), "console.log('ok');").unwrap();
    std::fs::write(workspace.join("app.json"), r#"{"main":"index.ts"}"#).unwrap();

    let cache_dir = temp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let cache_paths = artifact_cache_paths(&cache_dir, "v1", Some("linux-aarch64-musl"));
    let archive_size = package_target_artifact(
        &workspace,
        &workspace,
        &[],
        &["**/*".to_string()],
        &[],
        &cache_paths,
        "linux-aarch64-musl",
    )
    .unwrap();
    assert!(archive_size > 0);
}

#[test]
fn package_target_artifact_preserves_workspace_protocol_dependencies() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("package.json"),
        r#"{"name":"web","dependencies":{"tako.sh":"workspace:*"}}"#,
    )
    .unwrap();
    std::fs::write(workspace.join("src/app.ts"), "export default {};\n").unwrap();

    let cache_dir = temp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let cache_paths = artifact_cache_paths(&cache_dir, "v1", Some("linux-aarch64-musl"));
    let archive_size = package_target_artifact(
        &workspace,
        &workspace,
        &[],
        &["**/*".to_string()],
        &["**/node_modules/**".to_string()],
        &cache_paths,
        "linux-aarch64-musl",
    )
    .unwrap();
    assert!(archive_size > 0);

    let unpacked = temp.path().join("unpacked");
    BuildExecutor::extract_archive(&cache_paths.artifact_path, &unpacked).unwrap();
    let package_json = std::fs::read_to_string(unpacked.join("package.json")).unwrap();
    let package_json: serde_json::Value = serde_json::from_str(&package_json).unwrap();
    assert_eq!(
        package_json
            .get("dependencies")
            .and_then(|deps| deps.get("tako.sh"))
            .and_then(|value| value.as_str()),
        Some("workspace:*")
    );
}

#[test]
fn package_target_artifact_does_not_validate_workspace_protocol_dependencies() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("package.json"),
        r#"{"name":"web","dependencies":{"missing-pkg":"workspace:*"}}"#,
    )
    .unwrap();
    std::fs::write(workspace.join("src.ts"), "export default {};\n").unwrap();

    let cache_dir = temp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let cache_paths = artifact_cache_paths(&cache_dir, "v1", Some("linux-aarch64-musl"));
    let archive_size = package_target_artifact(
        &workspace,
        &workspace,
        &[],
        &["**/*".to_string()],
        &[],
        &cache_paths,
        "linux-aarch64-musl",
    )
    .unwrap();
    assert!(archive_size > 0);
}

#[test]
fn build_stage_summary_output_is_hidden_when_empty() {
    let summary: Vec<String> = vec![];
    assert_eq!(format_build_stages_summary_for_output(&summary, None), None);
}

#[test]
fn build_stage_summary_output_is_shown_when_non_empty() {
    let summary = vec!["Stage 'preset'".to_string(), "Stage 2".to_string()];
    assert_eq!(
        format_build_stages_summary_for_output(&summary, Some("linux-x86_64-glibc")),
        Some("Build stages for linux-x86_64-glibc: Stage 'preset' -> Stage 2".to_string())
    );
}

#[test]
fn prepare_build_phase_rejects_container_release_before_native_build() {
    let project = TempDir::new().unwrap();
    let mut tako_config = TakoToml::default();
    tako_config.container = Some("Dockerfile".to_string());

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(prepare_build_phase(
        project.path().to_path_buf(),
        project.path().to_path_buf(),
        project.path().to_path_buf(),
        "app".to_string(),
        "production".to_string(),
        tako_config,
        crate::config::SecretsStore::default(),
        "javascript/vite".to_string(),
        BuildAdapter::Node,
        vec![],
        vec![],
        None,
    ));
    let err = match result {
        Ok(_) => panic!("container deploy should not enter native build preparation"),
        Err(error) => error,
    };

    assert!(err.contains("Container deploys are not implemented yet"));
    assert!(err.contains("container = \"Dockerfile\""));
}

#[test]
fn prepare_build_phase_does_not_leave_unused_tmp_paths() {
    let _lock = crate::paths::test_tako_home_env_lock();
    let previous = std::env::var_os("TAKO_HOME");
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    unsafe {
        std::env::set_var("TAKO_HOME", home.path());
    }

    std::fs::write(project.path().join("package.json"), r#"{"name":"app"}"#).unwrap();
    std::fs::write(project.path().join("index.ts"), "export default {};\n").unwrap();

    let repo = "tako-sh/tako";
    let path = "presets/javascript.toml";
    let branch_sha = "d0ff9bec5b3d42a874b1bff544249b3a4c530d9f";
    let manifest = r#"
[vite]
dev = ["vite", "dev"]
"#;
    crate::build::preset_cache::write_cached(repo, branch_sha, path, manifest).unwrap();

    let mut tako_config = TakoToml::default();
    tako_config.build.run = Some("true".to_string());

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(prepare_build_phase(
        project.path().to_path_buf(),
        project.path().to_path_buf(),
        project.path().to_path_buf(),
        "app".to_string(),
        "production".to_string(),
        tako_config,
        crate::config::SecretsStore::default(),
        format!("javascript/vite@{branch_sha}"),
        BuildAdapter::Node,
        vec![(
            "local".to_string(),
            crate::config::ServerTarget {
                arch: "x86_64".to_string(),
                libc: "glibc".to_string(),
            },
        )],
        vec![ArtifactBuildGroup {
            build_target_label: "linux-x86_64-glibc".to_string(),
            cache_target_label: "linux-x86_64-glibc".to_string(),
            target_labels: vec!["linux-x86_64-glibc".to_string()],
            display_target_label: None,
        }],
        None,
    ));

    match previous {
        Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
        None => unsafe { std::env::remove_var("TAKO_HOME") },
    }

    let build_phase = result.unwrap();
    assert!(
        build_phase
            .artifacts_by_target
            .contains_key("linux-x86_64-glibc")
    );
    assert!(!project.path().join(".tako/tmp").exists());
}
