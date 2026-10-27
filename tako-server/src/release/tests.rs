use super::*;
use crate::isolation::fixture::TestDataDir as TempDir;

#[test]
#[cfg(unix)]
fn install_failure_reports_the_end_of_long_diagnostics() {
    use std::os::unix::process::ExitStatusExt;
    let stderr = format!("{}fatal dependency error", "x".repeat(1000));
    let message = format_process_failure(
        "production install",
        ExitStatus::from_raw(23 << 8),
        b"",
        stderr.as_bytes(),
    );
    assert!(message.ends_with("fatal dependency error"));
    assert!(message.contains("exit code 23"));
}

#[test]
fn app_runtime_data_paths_use_nested_app_and_tako_dirs() {
    let data_dir = Path::new("/opt/tako");
    let paths = app_runtime_data_paths(data_dir, "my-app/production");
    assert_eq!(
        paths.root,
        Path::new("/opt/tako/apps/my-app/production/data")
    );
    assert_eq!(
        paths.app,
        Path::new("/opt/tako/apps/my-app/production/data/app")
    );
    assert_eq!(
        paths.tako,
        Path::new("/opt/tako/apps/my-app/production/data/tako")
    );
}

#[test]
fn ensure_app_runtime_data_dirs_creates_both_directories() {
    let temp = TempDir::new().unwrap();
    let paths = ensure_app_runtime_data_dirs(temp.path(), "my-app/production").unwrap();
    assert!(paths.app.is_dir());
    assert!(paths.tako.is_dir());
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
#[cfg(unix)]
fn ensure_app_runtime_data_dirs_makes_app_data_group_writable() {
    let temp = TempDir::new().unwrap();
    let paths = ensure_app_runtime_data_dirs(temp.path(), "my-app/production").unwrap();

    assert_eq!(
        mode(&paths.root),
        if cfg!(target_os = "linux") {
            0o750
        } else {
            0o710
        }
    );
    assert_eq!(mode(&paths.app), 0o2770);
    assert_eq!(mode(&paths.tako), 0o700);
}

#[test]
#[cfg(unix)]
fn ensure_app_runtime_data_dirs_repairs_existing_app_data_files() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let paths = app_runtime_data_paths(temp.path(), "my-app/production");
    std::fs::create_dir_all(&paths.app).unwrap();
    std::fs::create_dir_all(&paths.tako).unwrap();
    let db_path = paths.app.join("mission.sqlite");
    let wal_path = paths.app.join("mission.sqlite-wal");
    std::fs::write(&db_path, "db").unwrap();
    std::fs::write(&wal_path, "wal").unwrap();
    std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&wal_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    ensure_app_runtime_data_dirs(temp.path(), "my-app/production").unwrap();

    assert_eq!(mode(&db_path) & 0o660, 0o660);
    assert_eq!(mode(&wal_path) & 0o660, 0o660);
}

#[test]
fn inject_app_data_dir_env_sets_tako_data_dir() {
    let mut env = HashMap::new();
    let paths = AppRuntimeDataPaths {
        root: PathBuf::from("/tmp/app/data"),
        app: PathBuf::from("/tmp/app/data/app"),
        tako: PathBuf::from("/tmp/app/data/tako"),
    };
    inject_app_data_dir_env(&mut env, &paths);
    assert_eq!(
        env.get(TAKO_APP_DATA_DIR_ENV).map(String::as_str),
        Some("/tmp/app/data/app")
    );
}

#[test]
fn apply_release_runtime_to_config_sets_container_launch() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("app.json"),
        r#"{"protocol_version":0,"release_kind":"container","app_name":"my-app","environment":"production","version":"v1","runtime":"container","main":"","idle_timeout":300,"container_file":"Dockerfile","container_port":3000}"#,
    )
    .unwrap();

    let mut config = AppConfig::default();
    apply_release_runtime_to_config(&mut config, temp.path().to_path_buf(), None).unwrap();

    assert_eq!(
        config.launch,
        AppLaunch::Container {
            image: "tako/my-app-production:v1".to_string(),
            port: 3000
        }
    );
    assert!(config.command.is_empty());
}

#[tokio::test]
async fn prepare_release_runtime_skips_runtime_install_for_explicit_start() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("app.json"),
        r#"{"protocol_version":0,"app_name":"my-app","environment":"production","version":"v1","runtime":"","main":"","start":["./app"],"idle_timeout":300}"#,
    )
    .unwrap();

    let resolved = prepare_release_runtime(temp.path(), &HashMap::new(), temp.path(), None)
        .await
        .unwrap();

    assert_eq!(resolved, None);
}

#[tokio::test]
async fn release_preparation_propagates_runtime_and_package_manager_failures() {
    for (runtime_version, pm, pm_version) in [(".", "bun", "1.0.0"), ("1.0.0", "node", ".")] {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtimes/bun/1.0.0");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("bun"), "cached").unwrap();
        let manifest = serde_json::json!({
            "protocol_version": 0, "app_name": "my-app", "environment": "production",
            "version": "v1", "runtime": "bun", "main": "index.js", "idle_timeout": 300,
            "runtime_version": runtime_version, "package_manager": pm,
            "package_manager_version": pm_version
        });
        std::fs::write(temp.path().join("app.json"), manifest.to_string()).unwrap();
        let error = prepare_release_runtime(temp.path(), &HashMap::new(), temp.path(), None)
            .await
            .unwrap_err();
        assert!(error.contains("Failed to install"), "{error}");
        if runtime_version == "." {
            assert!(
                resolve_release_runtime_bin(temp.path(), temp.path())
                    .await
                    .unwrap_err()
                    .contains("Failed to install")
            );
        }
    }
}

#[tokio::test]
async fn production_install_command_does_not_inherit_server_env() {
    let parent_secret = EnvGuard::set("TAKO_SERVER_PARENT_SECRET", "should-not-leak");
    let temp = TempDir::new().unwrap();
    let mut env = HashMap::new();
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }

    let output = run_production_install_command(
        "printf %s \"${TAKO_SERVER_PARENT_SECRET:-missing}\"",
        temp.path(),
        &env,
        None,
    )
    .await
    .unwrap();

    drop(parent_secret);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "missing");
}

#[tokio::test]
#[cfg(unix)]
async fn failed_production_dependencies_abort_release_preparation() {
    use std::os::unix::fs::PermissionsExt;
    for pm in ["pnpm", "yarn"] {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtimes/bun/1.0.0");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("bun"), "cached").unwrap();
        let pm_bin = runtime_dir.join(pm);
        std::fs::write(
            &pm_bin,
            "#!/bin/sh\necho dependency-install-failed >&2\nexit 23\n",
        )
        .unwrap();
        std::fs::set_permissions(&pm_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let manifest = serde_json::json!({
            "protocol_version": 0, "app_name": "my-app", "environment": "production",
            "version": "v1", "runtime": "bun", "main": "index.js", "idle_timeout": 300,
            "runtime_version": "1.0.0", "package_manager": pm
        });
        std::fs::write(temp.path().join("app.json"), manifest.to_string()).unwrap();
        let error = prepare_release_runtime(temp.path(), &HashMap::new(), temp.path(), None)
            .await
            .unwrap_err();
        assert!(
            error.contains("production install (exit code 23)"),
            "{error}"
        );
        assert!(error.contains("dependency-install-failed"), "{error}");
    }
}

#[tokio::test]
#[cfg(unix)]
async fn production_install_applies_process_isolation() {
    let temp = TempDir::new().unwrap();
    let mut env = HashMap::new();
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }
    let isolation = ProcessIsolation {
        resource_limits: tako_spawn::ResourceLimits {
            open_files: None,
            processes: None,
            address_space_bytes: None,
        },
        umask: Some(0o027),
        ..Default::default()
    };

    let output = run_production_install_command("umask", temp.path(), &env, Some(isolation))
        .await
        .unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0027");
}

struct EnvGuard {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            unsafe { std::env::set_var(self.name, value) };
        } else {
            unsafe { std::env::remove_var(self.name) };
        }
    }
}

#[test]
fn release_manifest_metadata_preserves_custom_description_and_source() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("app.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "description": "Fix reconnect handling",
            "commit_message": "feat: live stores",
            "git_dirty": true,
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        read_release_manifest_metadata(&path),
        (
            Some("feat: live stores".to_string()),
            Some(true),
            Some("Fix reconnect handling".to_string()),
        )
    );
}
