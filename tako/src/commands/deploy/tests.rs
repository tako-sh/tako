use super::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn deploy_config_paths_are_derived_from_remote_base() {
    let cfg = DeployConfig {
        app_name: "my-app".to_string(),
        version: "v1".to_string(),
        remote_base: "/opt/tako/apps/my-app".to_string(),
        routes: vec![],
        env_vars: HashMap::new(),
        secrets_hash: String::new(),
        main: "index.ts".to_string(),
        use_unified_target_process: false,
        release_command: None,
        leader_server: String::new(),
    };
    assert_eq!(cfg.release_dir(), "/opt/tako/apps/my-app/releases/v1");
    assert_eq!(cfg.current_link(), "/opt/tako/apps/my-app/current");
    assert_eq!(cfg.shared_dir(), "/opt/tako/apps/my-app/shared");
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
