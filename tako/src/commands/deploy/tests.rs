use super::*;
use crate::config::EnvConfig;
use std::fs;
use tempfile::TempDir;

#[test]
fn release_command_payload_pins_the_secret_snapshot() {
    let cfg = DeployConfig {
        app_name: "my-app/production".to_string(),
        version: "v1".to_string(),
        routes: vec![],
        source_ip: tako_core::SourceIpMode::Auto,
        secrets_digest: Some("digest".to_string()),
        storages: HashMap::new(),
        databases: HashMap::from([(
            "main".into(),
            tako_core::DatabaseBinding {
                name: "orders".into(),
                generation: "0123456789abcdef0123456789abcdef".into(),
                peer_key: None,
                engine: tako_core::DatabaseEngine::Turso,
            },
        )]),
        channel_database: Some(tako_core::DatabaseBinding {
            name: "replay".into(),
            generation: "0123456789abcdef0123456789abcdef".into(),
            peer_key: None,
            engine: tako_core::DatabaseEngine::Turso,
        }),
        workflows_database: None,
        ssl: tako_core::SslBinding::default(),
        force: true,
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
        secrets_digest,
        databases,
        force,
        ..
    }) = cfg.release_command_payload("/opt/tako/apps/my-app/production/releases/v1")
    else {
        panic!("expected run_release command payload");
    };

    assert_eq!(app, "my-app/production");
    assert_eq!(version, "v1");
    assert_eq!(path, "/opt/tako/apps/my-app/production/releases/v1");
    assert_eq!(command_line, "bun run migrate");
    assert!(force);
    assert_eq!(secrets_digest.as_deref(), Some("digest"));
    assert_eq!(
        databases, cfg.databases,
        "release commands get only SQL bindings"
    );
    let full = cfg.deploy_databases(true).unwrap();
    assert_eq!(full.len(), 2);
    let scoped = cfg.deploy_databases(false).unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[tako_core::CHANNEL_REPLAY_BINDING].name, "replay");
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

#[test]
fn releases_need_a_workflow_database_only_when_they_run_workflows() {
    let temp = TempDir::new().unwrap();
    let config = multi_server_js_config();
    let needs = |dir: &std::path::Path, config: &TakoToml, adapter| {
        config::needs_workflows(dir, config, adapter)
    };
    assert!(!needs(temp.path(), &config, BuildAdapter::Bun));
    // Type declarations are not workflow sources.
    for suffix in ["ts", "mts", "cts"] {
        write_workflow(
            &temp,
            &format!("network.d.{suffix}"),
            "export type Remote = unknown;",
        );
    }
    assert!(!needs(temp.path(), &config, BuildAdapter::Bun));
    write_workflow(
        &temp,
        "send-email.ts",
        "export default defineWorkflow('send-email', { handler: async () => {} });",
    );
    assert!(needs(temp.path(), &config, BuildAdapter::Bun));

    // A Go project runs workflows from its `cmd/worker`.
    let go = TempDir::new().unwrap();
    let go_config = TakoToml::parse("name = 'api'\nruntime = 'go'\n").unwrap();
    assert!(!needs(go.path(), &go_config, BuildAdapter::Go));
    fs::create_dir_all(go.path().join("cmd/worker")).unwrap();
    fs::write(go.path().join("cmd/worker/main.go"), "package main\n").unwrap();
    assert!(needs(go.path(), &go_config, BuildAdapter::Go));

    // A container runs workflows when it configures a workflow command.
    let container = TempDir::new().unwrap();
    let image = TakoToml::parse("name = 'api'\nimage = 'ghcr.io/acme/api:1'\n").unwrap();
    assert!(!needs(container.path(), &image, BuildAdapter::Unknown));
    let worker = TakoToml::parse(
        "name = 'api'\nimage = 'ghcr.io/acme/api:1'\n[workflows.jobs]\nrun = ['worker']\n",
    )
    .unwrap();
    assert!(needs(container.path(), &worker, BuildAdapter::Unknown));
}

fn write_workflow(temp: &TempDir, name: &str, source: &str) {
    let workflows_dir = temp.path().join("src/workflows");
    fs::create_dir_all(&workflows_dir).unwrap();
    fs::write(workflows_dir.join(name), source).unwrap();
}

fn multi_server_js_config() -> TakoToml {
    TakoToml {
        app_root: Some("src".to_string()),
        envs: HashMap::from([(
            "production".to_string(),
            EnvConfig {
                servers: vec!["sfo".to_string(), "iad".to_string()],
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

#[test]
fn native_releases_need_channel_replay_only_with_a_tako_sdk() {
    let temp = TempDir::new().unwrap();
    let config = TakoToml::parse("name = 'api'\nruntime = 'go'\n").unwrap();
    let needs = || config::needs_channel_replay(temp.path(), &config, BuildAdapter::Go);
    fs::write(temp.path().join("go.mod"), "module api\n\ngo 1.22\n").unwrap();
    assert!(!needs());
    fs::write(
        temp.path().join("go.mod"),
        "module api\n\ngo 1.22\n\nrequire tako.sh v0.4.1\n",
    )
    .unwrap();
    assert!(needs());
    // A custom-built Rust binary counts before its first build writes Cargo.lock.
    let rust = TempDir::new().unwrap();
    fs::write(
        rust.path().join("Cargo.toml"),
        "[package]\nname = 'api'\n[dependencies]\ntako = '0.3'\n",
    )
    .unwrap();
    assert!(config::needs_channel_replay(
        rust.path(),
        &config,
        BuildAdapter::Unknown
    ));
}
