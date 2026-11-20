use crate::support::*;

#[test]
fn dev_stop_all_attempts_remaining_apps_and_reports_every_failure() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    let attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = attempts.clone();
    let _fake = FakeDevServer::start_with_handler(&tako_home, move |request| match request["type"]
        .as_str()?
    {
        "ListRegisteredApps" => {
            let apps: Vec<_> = [
                ("broken-first", "test-e2e"),
                ("healthy", "test-e2e"),
                ("broken-last", "test-e2e"),
                ("development", "development"),
            ]
            .into_iter()
            .map(|(name, environment)| {
                serde_json::json!({
                    "app_name": name, "environment": environment,
                    "config_file": format!("/{name}/tako.toml"),
                    "config_path": format!("{environment}:/{name}/tako.toml"),
                    "project_dir": format!("/{name}"), "upstream_port": 0
                })
            })
            .collect();
            Some(serde_json::json!({"type": "RegisteredApps", "apps": apps}))
        }
        "UnregisterApp" => {
            let key = request["config_path"].as_str().unwrap();
            recorded.lock().unwrap().push(key.to_string());
            Some(if key.contains("broken") {
                serde_json::json!({"type": "Error", "message": "Compose cleanup failed"})
            } else {
                serde_json::json!({"type": "AppUnregistered", "config_path": key})
            })
        }
        _ => None,
    })
    .unwrap();
    let out = run_tako_with_env(
        &["--ci", "dev", "stop", "--all", "--env", "test-e2e"],
        temp.path(),
        &home,
        &tako_home,
    );
    assert!(!out.status.success());
    assert_eq!(
        *attempts.lock().unwrap(),
        [
            "test-e2e:/broken-first/tako.toml",
            "test-e2e:/healthy/tako.toml",
            "test-e2e:/broken-last/tako.toml"
        ]
    );
    let stderr = stderr_str(&out);
    for expected in [
        "Stopped healthy",
        "broken-first",
        "broken-last",
        "Compose cleanup failed",
    ] {
        assert!(stderr.contains(expected), "{stderr}");
    }
}

#[test]
fn dev_doctor_prints_info() {
    let _guard = dev_daemon_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    setup_minimal_bun_project(&project_dir);
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();

    let Some(_fake) = FakeDevServer::start(&tako_home) else {
        return;
    };

    let out = run_tako_with_env(&["doctor"], &project_dir, &home, &tako_home);
    assert!(out.status.success(), "doctor failed: {}", stderr_str(&out));
    let combined = format!("{}{}", stdout_str(&out), stderr_str(&out));
    assert!(
        combined.contains("Development server"),
        "unexpected doctor output: {}",
        combined
    );
    assert!(
        combined.contains("Listen"),
        "expected Listen row: {}",
        combined
    );
    assert!(
        combined.contains("Apps"),
        "expected Apps section: {}",
        combined
    );
}
