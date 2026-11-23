use super::*;
use crate::isolation::fixture::TestDataDir as TempDir;

/// Serves one workflow database for every app of `state`; apps keep their
/// own runs in it. The server must outlive the test's workflow runtimes.
pub(super) fn serve_workflows(state: &ServerState) -> tako_db::testing::TestServer {
    let server = tako_db::testing::TestServer::start();
    let database = server.database("tests#workflows");
    let (url, token) = (server.url.clone(), database.token);
    state.workflows.set_store_resolver(Arc::new(move |app| {
        Some(tako_workflows::WorkflowStoreConfig::hosted(
            url.clone(),
            token.clone(),
            app,
        ))
    }));
    server
}

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
    let _workflow_db = serve_workflows(&state);

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
    for _ in 0..20 {
        if !state.workflows.has(app_id) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("deploying a release without workflows/ should retire the old workflow runtime");
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
    let _workflow_db = serve_workflows(&state);

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
        std::fs::write(
            release.join(workflows_dir).join("send.ts"),
            r#"export default defineWorkflow("send", { handler: async () => {} });"#,
        )
        .unwrap();
        std::fs::create_dir_all(release.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
        std::fs::write(
            release.join("node_modules/tako.sh/dist/entrypoints/bun-worker.mjs"),
            "export default {};",
        )
        .unwrap();
        std::fs::write(
            release.join("app.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "protocol_version": tako_core::PROTOCOL_VERSION,
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
    let _workflow_db = serve_workflows(&state);

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
    let _workflow_db = serve_workflows(&state);

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
            "protocol_version": tako_core::PROTOCOL_VERSION,
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

    let captured = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let value = std::fs::read_to_string(&env_capture).ok();
            if let Some(value) = value
                && !value.trim().is_empty()
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    state.workflows.retire(app_id);
    let captured = captured.expect("worker should record its environment");
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
async fn sync_app_workflows_registers_go_worker_binary_from_manifest() {
    let temp = TempDir::new().unwrap();
    let app_id = "go-app/production";
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
    let _workflow_db = serve_workflows(&state);

    let release = temp
        .path()
        .join("apps")
        .join("go-app")
        .join("production")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("worker"), "cat <&3 >/dev/null\nsleep 600\n").unwrap();
    std::fs::write(
        release.join("app.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "protocol_version": tako_core::PROTOCOL_VERSION,
            "runtime": "go",
            "main": "app",
            "workflow_worker_main": "worker",
            "idle_timeout": 300,
            "env_vars": {
                "TAKO_BUILD": "v1"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    state
        .sync_app_workflows(app_id, &release, Some("/bin/sh"))
        .await;

    assert!(
        state.workflows.has(app_id),
        "Go releases with workflow_worker_main should register a worker supervisor"
    );
}

#[tokio::test]
async fn deploys_of_workflow_releases_refuse_an_environment_without_a_workflow_database() {
    let temp = TempDir::new().unwrap();
    let app_id = "go-app/production";
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
    let release = temp.path().join("apps/go-app/production/releases/v1");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("worker"), "sleep 600\n").unwrap();
    std::fs::write(
        release.join("app.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "protocol_version": tako_core::PROTOCOL_VERSION,
            "runtime": "go",
            "main": "app",
            "workflow_worker_main": "worker",
            "idle_timeout": 300,
        }))
        .unwrap(),
    )
    .unwrap();
    let (secrets, storages, databases, kv) = (
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let prepare = |require_store| {
        state.prepare_app_workflows(
            app_id,
            &release,
            Some("/bin/sh"),
            crate::server_state::WorkflowBindings {
                secrets: &secrets,
                storages: &storages,
                databases: &databases,
                kv: &kv,
                store: None,
                require_store,
            },
            tako_workflows::manager::ScheduleActivation::Deploy,
        )
    };
    // Only a full client can provision the database, so a deploy must not
    // look successful without it; recovery keeps the app up without workflows.
    let Err(error) = prepare(true).await else {
        panic!("a deploy without a workflow database succeeded");
    };
    assert!(
        error.to_string().contains("no workflow database"),
        "{error}"
    );
    assert!(prepare(false).await.unwrap().is_none());
}

#[tokio::test]
async fn a_workflows_directory_without_definitions_runs_no_workflows() {
    let temp = TempDir::new().unwrap();
    let app_id = "js-app/production";
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
    let release = temp.path().join("apps/js-app/production/releases/v1");
    write_js_workflow_scaffold(&release);
    write_release_manifest(&release, "bun", "index.js", &["bun", "{main}"], None, 300);
    // The deploy client provisions no workflow database for these files.
    std::fs::remove_file(release.join("src/workflows/send.ts")).unwrap();
    std::fs::write(release.join("src/workflows/README.md"), "notes").unwrap();
    std::fs::write(release.join("src/workflows/_shared.ts"), "export {};").unwrap();
    let (secrets, storages, databases, kv) = (
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let prepared = state
        .prepare_app_workflows(
            app_id,
            &release,
            None,
            crate::server_state::WorkflowBindings {
                secrets: &secrets,
                storages: &storages,
                databases: &databases,
                kv: &kv,
                store: None,
                require_store: true,
            },
            tako_workflows::manager::ScheduleActivation::Deploy,
        )
        .await;
    assert!(matches!(prepared, Ok(None)));
}

#[tokio::test(flavor = "multi_thread")]
async fn operator_workflow_commands_inspect_retry_and_cancel_runs() {
    let temp = TempDir::new().unwrap();
    let app_id = "wf-ops/production";
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
    state.app_manager.register_app(AppConfig {
        name: "wf-ops".to_string(),
        environment: "production".to_string(),
        version: "v1".to_string(),
        ..Default::default()
    });
    let call = |command| state.handle_command(command);
    let list = |limit| Command::ListWorkflowRuns {
        app: app_id.into(),
        status: None,
        name: None,
        limit,
    };
    let get = |id: &str| Command::GetWorkflowRun {
        app: app_id.into(),
        id: id.into(),
    };
    let retry = |id: &str| Command::RetryWorkflowRun {
        app: app_id.into(),
        id: id.into(),
    };
    let cancel = |id: &str| Command::CancelWorkflowRun {
        app: app_id.into(),
        id: id.into(),
    };
    fn data(response: Response) -> Value {
        match response {
            Response::Ok { data } => data,
            Response::Error { .. } => panic!("expected workflow command to succeed"),
        }
    }
    fn error(response: Response) -> String {
        response.error_message().expect("expected an error").into()
    }

    for command in [list(None), get("r1"), retry("r1"), cancel("r1")] {
        assert!(
            crate::operations::command_is_read_only(&command)
                == matches!(
                    command,
                    Command::ListWorkflowRuns { .. } | Command::GetWorkflowRun { .. }
                )
        );
    }
    assert_eq!(
        error(
            call(Command::GetWorkflowRun {
                app: "ghost/production".into(),
                id: "r1".into(),
            })
            .await
        ),
        "App not found: ghost/production"
    );

    // Without a workflow database the app has no runs to inspect.
    assert!(
        error(call(list(None)).await).contains("has no workflow database"),
        "listing needs the environment's workflow database"
    );

    // The hosted client blocks, so the test drives it off the runtime.
    let server = tako_db::testing::TestServer::start();
    let database = server.database(&tako_core::workflows_grant_scope(app_id));
    let store = tako_workflows::WorkflowStoreConfig::hosted(
        server.url.clone(),
        database.token.clone(),
        app_id,
    );
    {
        let store = store.clone();
        state
            .workflows
            .set_store_resolver(Arc::new(move |app| (app == app_id).then(|| store.clone())));
    }
    let empty = data(call(list(None)).await);
    assert_eq!(empty["runs"], serde_json::json!([]));
    assert!(data(call(get("r1")).await)["run"].is_null());
    assert_eq!(error(call(retry("r1")).await), "Workflow run not found: r1");

    let (db, owner) = tokio::task::block_in_place(|| {
        let db = tako_workflows::RunsDb::open(store).unwrap();
        for id in ["r1", "r2"] {
            let options = tako_core::EnqueueOpts {
                id: Some(id.into()),
                ..Default::default()
            };
            db.enqueue("work", &serde_json::json!({"n": id}), &options)
                .unwrap();
        }
        let owner = db
            .claim("worker", &["work".into()], 60_000)
            .unwrap()
            .unwrap();
        (db, owner)
    });
    assert_eq!(owner.id, "r1");

    let listed: tako_core::ListWorkflowRunsResponse =
        serde_json::from_value(data(call(list(Some(1))).await)).unwrap();
    assert_eq!(listed.app, app_id);
    assert_eq!(listed.runs.len(), 1);
    assert!(listed.has_more);
    assert_eq!(
        error(call(list(Some(0))).await),
        "Workflow run list limit must be between 1 and 200"
    );
    let shown: tako_core::GetWorkflowRunResponse =
        serde_json::from_value(data(call(get("r1")).await)).unwrap();
    let shown = shown.run.unwrap();
    assert_eq!(shown.status, tako_core::WorkflowRunStatus::Running);
    assert_eq!(shown.payload.value, serde_json::json!({"n": "r1"}));

    let cancelled: tako_core::WorkflowRunTransition =
        serde_json::from_value(data(call(cancel("r1")).await)).unwrap();
    assert_eq!(
        (cancelled.previous_status, cancelled.status),
        (
            tako_core::WorkflowRunStatus::Running,
            tako_core::WorkflowRunStatus::Cancelled
        )
    );
    assert!(
        tokio::task::block_in_place(|| db
            .complete("r1", &owner.worker_id, &serde_json::Value::Null)
            .is_err()),
        "the cancelled attempt is fenced"
    );
    assert_eq!(
        error(call(cancel("r1")).await),
        "Workflow run r1 already finished as cancelled; only pending or running runs can be cancelled"
    );
    assert_eq!(
        error(call(retry("r2")).await),
        "Workflow run r2 is pending; only dead or cancelled runs can be retried"
    );

    // Upgrades pause repairs but not inspection.
    data(
        call(Command::EnterUpgrading {
            owner: "ops".into(),
        })
        .await,
    );
    assert!(error(call(retry("r1")).await).contains("Server is upgrading"));
    assert!(error(call(cancel("r2")).await).contains("Server is upgrading"));
    data(call(get("r1")).await);
    data(
        call(Command::ExitUpgrading {
            owner: "ops".into(),
        })
        .await,
    );

    let cancelled = data(call(cancel("r2")).await);
    assert_eq!(cancelled["previous_status"], "pending");
    let retried: tako_core::WorkflowRunTransition =
        serde_json::from_value(data(call(retry("r1")).await)).unwrap();
    assert_eq!(retried.status, tako_core::WorkflowRunStatus::Pending);
    let again = tokio::task::block_in_place(|| {
        db.claim("worker", &["work".into()], 60_000)
            .unwrap()
            .unwrap()
    });
    assert_eq!((again.id.as_str(), again.attempts), ("r1", 1));
}
