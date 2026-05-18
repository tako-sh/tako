use super::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn release_command_payload_includes_deploy_secrets() {
    let cfg = DeployConfig {
        app_name: "my-app/production".to_string(),
        version: "v1".to_string(),
        routes: vec![],
        source_ip: tako_core::SourceIpMode::Auto,
        secrets: HashMap::from([("DATABASE_URL".to_string(), "postgres://new".to_string())]),
        storages: HashMap::new(),
        dns: None,
        secrets_hash: String::new(),
        main: "index.ts".to_string(),
        use_unified_target_process: false,
        release_command: Some("bun run migrate".to_string()),
        leader_server: "prod".to_string(),
    };

    let Some(tako_core::Command::RunRelease {
        app,
        version,
        path,
        command_line,
        secrets,
        ..
    }) = cfg.release_command_payload("/opt/tako/apps/my-app/production/releases/v1")
    else {
        panic!("expected run_release command payload");
    };

    assert_eq!(app, "my-app/production");
    assert_eq!(version, "v1");
    assert_eq!(path, "/opt/tako/apps/my-app/production/releases/v1");
    assert_eq!(command_line, "bun run migrate");
    assert_eq!(
        secrets.get("DATABASE_URL").map(String::as_str),
        Some("postgres://new")
    );
}

#[test]
fn source_bundle_root_falls_back_to_runtime_project_root_without_git() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("app");
    std::fs::create_dir_all(&project_dir).unwrap();
    // No lockfile anywhere → falls back to project_dir itself
    assert_eq!(source_bundle_root(&project_dir, "bun"), project_dir);
}

#[test]
fn source_bundle_root_walks_up_to_lockfile_without_git() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("monorepo");
    let project_dir = root.join("apps/web");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(root.join("bun.lock"), "").unwrap();
    // No git, but lockfile is at the monorepo root → returns lockfile root
    assert_eq!(source_bundle_root(&project_dir, "bun"), root);
}

#[test]
fn acquire_project_deploy_lock_writes_current_pid() {
    let temp = TempDir::new().unwrap();
    let _lock = acquire_project_deploy_lock(temp.path()).unwrap();

    let pid_path = deploy_lock_path(temp.path());
    assert_eq!(
        fs::read_to_string(pid_path).unwrap().trim(),
        std::process::id().to_string()
    );
}

#[test]
fn acquire_project_deploy_lock_rejects_second_holder() {
    let temp = TempDir::new().unwrap();
    let _lock = acquire_project_deploy_lock(temp.path()).unwrap();

    let err = acquire_project_deploy_lock(temp.path()).unwrap_err();
    assert!(err.contains("Another deploy is already running"));
    assert!(err.contains(&std::process::id().to_string()));
}

#[test]
fn acquire_project_deploy_lock_allows_reacquire_after_drop() {
    let temp = TempDir::new().unwrap();
    let first = acquire_project_deploy_lock(temp.path()).unwrap();
    drop(first);

    let second = acquire_project_deploy_lock(temp.path()).unwrap();
    let pid_path = deploy_lock_path(temp.path());
    assert_eq!(
        fs::read_to_string(pid_path).unwrap().trim(),
        std::process::id().to_string()
    );
    drop(second);
}
