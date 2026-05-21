use super::*;

#[tokio::test]
async fn sync_app_workflows_restarts_existing_entry_and_stops_removed_workflows() {
    let temp = TempDir::new().unwrap();
    let app_id = "workflow-app/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_v1 = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v1");
    write_js_workflow_scaffold(&release_v1);
    write_release_manifest(
        &release_v1,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    state.sync_app_workflows(app_id, &release_v1, None).await;
    let first = state
        .workflows
        .supervisor_for(app_id)
        .expect("v1 should register workflows");

    let release_v2 = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v2");
    write_js_workflow_scaffold(&release_v2);
    write_release_manifest(
        &release_v2,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    state.sync_app_workflows(app_id, &release_v2, None).await;
    let second = state
        .workflows
        .supervisor_for(app_id)
        .expect("v2 should replace workflows");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "redeploy should replace the workflow supervisor"
    );

    let release_v3 = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v3");
    std::fs::create_dir_all(&release_v3).unwrap();
    write_release_manifest(
        &release_v3,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    state.sync_app_workflows(app_id, &release_v3, None).await;
    assert!(
        !state.workflows.has(app_id),
        "deploying a release without workflows/ should stop the old workflow runtime"
    );
}

#[tokio::test]
async fn sync_app_workflows_uses_manifest_app_root() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    for (index, (app_root, workflows_dir)) in [(".", "workflows"), ("app", "app/workflows")]
        .into_iter()
        .enumerate()
    {
        let app_id = format!("workflow-app-{index}/production");
        let release = temp
            .path()
            .join("apps")
            .join(format!("workflow-app-{index}"))
            .join("production")
            .join("releases")
            .join("v1");
        std::fs::create_dir_all(release.join(workflows_dir)).unwrap();
        std::fs::create_dir_all(release.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
        std::fs::write(
            release.join("node_modules/tako.sh/dist/entrypoints/bun-worker.mjs"),
            "export default {};",
        )
        .unwrap();
        std::fs::write(
            release.join("app.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "runtime": "bun",
                "main": "index.js",
                "idle_timeout": 300,
                "env_vars": {
                    "TAKO_APP_ROOT": app_root
                }
            }))
            .unwrap(),
        )
        .unwrap();

        state.sync_app_workflows(&app_id, &release, None).await;
        assert!(
            state.workflows.has(&app_id),
            "release with TAKO_APP_ROOT={app_root:?} should register workflows"
        );
    }
}

#[tokio::test]
async fn sync_app_workflows_respects_manifest_app_dir_for_workspace_layouts() {
    let temp = TempDir::new().unwrap();
    let app_id = "demo/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release = temp
        .path()
        .join("apps")
        .join("demo")
        .join("production")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release).unwrap();
    let app_dir = "examples/javascript/demo";
    write_js_workflow_scaffold_at(&release, app_dir);
    write_release_manifest_with_app_dir(
        &release,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
        app_dir,
    );

    state.sync_app_workflows(app_id, &release, None).await;
    assert!(
        state.workflows.has(app_id),
        "workspace-layout deploys should register workflows using manifest.app_dir"
    );
}

#[tokio::test]
async fn sync_app_workflows_injects_release_env_and_app_data_dir_into_worker() {
    let temp = TempDir::new().unwrap();
    let app_id = "workflow-app/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v1");
    write_js_workflow_scaffold(&release);
    let env_capture = temp.path().join("worker-env.txt");
    let worker_entry = release.join("node_modules/tako.sh/dist/entrypoints/bun-worker.mjs");
    std::fs::write(
        &worker_entry,
        format!(
            "cat <&3 >/dev/null\nprintf '%s\\n' \"$TAKO_BUILD|$CUSTOM_ENV|$TAKO_DATA_DIR|$TAKO_APP_NAME\" > {}\n",
            env_capture.display()
        ),
    )
    .unwrap();
    std::fs::write(
        release.join("app.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "runtime": "bun",
            "main": "index.js",
            "idle_timeout": 300,
            "env_vars": {
                "TAKO_BUILD": "v1",
                "CUSTOM_ENV": "worker-visible"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    state
        .sync_app_workflows(app_id, &release, Some("/bin/sh"))
        .await;
    let supervisor = state
        .workflows
        .supervisor_for(app_id)
        .expect("release with workflows should register worker supervisor");
    supervisor.wake().unwrap();

    let captured = (0..50)
        .find_map(|_| {
            let value = std::fs::read_to_string(&env_capture).ok();
            if let Some(value) = value
                && !value.trim().is_empty()
            {
                return Some(value);
            }
            std::thread::sleep(Duration::from_millis(10));
            None
        })
        .expect("worker should record its environment");
    let expected_data_dir = temp
        .path()
        .join("apps")
        .join(app_id)
        .join("data")
        .join("app");
    assert_eq!(
        captured.trim(),
        format!(
            "v1|worker-visible|{}|workflow-app/production",
            expected_data_dir.display()
        )
    );
}

#[tokio::test]
async fn update_secrets_restarts_workflows_even_without_http_instances() {
    let temp = TempDir::new().unwrap();
    let app_id = "workflow-app/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v1");
    write_js_workflow_scaffold(&release_dir);
    write_release_manifest(
        &release_dir,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    let app = state.app_manager.register_app(AppConfig {
        name: "workflow-app".to_string(),
        environment: "production".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 0,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    state.sync_app_workflows(app_id, &release_dir, None).await;

    let first = state
        .workflows
        .supervisor_for(app_id)
        .expect("initial workflow registration should succeed");
    let new_secrets: HashMap<String, String> = [("API_KEY".to_string(), "rotated".to_string())]
        .into_iter()
        .collect();

    let response = state
        .handle_command(Command::UpdateSecrets {
            app: app_id.to_string(),
            secrets: new_secrets.clone(),
        })
        .await;

    assert!(matches!(response, Response::Ok { .. }));
    let second = state
        .workflows
        .supervisor_for(app_id)
        .expect("workflow runtime should still be registered after secret rotation");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "secret rotation should replace the workflow supervisor even with zero HTTP instances"
    );
    assert_eq!(state.state_store.get_secrets(app_id).unwrap(), new_secrets);
    assert_eq!(
        app.config.read().secrets.get("API_KEY"),
        Some(&"rotated".to_string())
    );
}
