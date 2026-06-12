use super::*;

#[test]
fn resolve_dev_worker_command_returns_none_without_workflows_dir() {
    let temp = TempDir::new().unwrap();
    let cmd = resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Bun);
    assert!(cmd.is_none());
}

#[test]
fn resolve_dev_worker_command_returns_none_for_non_js_runtime() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src").join("workflows")).unwrap();
    assert!(resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Unknown).is_none());
}

#[test]
fn resolve_dev_worker_command_go_points_at_cmd_worker() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("cmd").join("worker")).unwrap();
    std::fs::write(
        temp.path().join("cmd").join("worker").join("main.go"),
        "package main",
    )
    .unwrap();

    let cmd = resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Go).unwrap();

    assert_eq!(cmd, vec!["go", "run", "./cmd/worker"]);
}

#[test]
fn resolve_dev_worker_command_bun_points_at_sdk_worker_entrypoint() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src").join("workflows")).unwrap();
    let cmd = resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Bun).unwrap();
    assert_eq!(cmd[0], "bun");
    assert!(cmd.iter().any(|a| a.contains("entrypoints/bun-worker.mjs")));
    assert!(!cmd.iter().any(|a| a.contains("{main}")));
}

#[test]
fn resolve_dev_worker_command_node_uses_strip_types_and_worker_entrypoint() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src").join("workflows")).unwrap();
    let cmd = resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Node).unwrap();
    assert_eq!(cmd[0], "node");
    assert!(cmd.iter().any(|a| a == "--experimental-strip-types"));
    assert!(
        cmd.iter()
            .any(|a| a.contains("entrypoints/node-worker.mjs"))
    );
}

#[test]
fn resolve_dev_worker_command_uses_configured_app_root() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("app").join("workflows")).unwrap();

    assert!(resolve_dev_worker_command(temp.path(), "app", BuildAdapter::Bun).is_some());
    assert!(resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Bun).is_none());
}
