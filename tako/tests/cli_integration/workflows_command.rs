use crate::support::*;

fn project() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();
    fs::write(tako_home.join("config.toml"), "").unwrap();
    fs::write(
        temp.path().join("tako.toml"),
        r#"
name = "demo"
runtime = "bun"
main = "index.ts"

[envs.production]
route = "demo.example.com"
"#,
    )
    .unwrap();
    (temp, home, tako_home)
}

#[test]
fn workflows_json_failure_without_a_network_is_a_structured_error() {
    let (temp, home, tako_home) = project();
    for args in [
        &["--json", "workflow", "list", "--env", "production"][..],
        &["--json", "workflow", "show", "run_1", "--env", "production"][..],
        &[
            "--json",
            "workflow",
            "retry",
            "run_1",
            "--env",
            "production",
        ][..],
        &[
            "--json",
            "workflow",
            "cancel",
            "run_1",
            "--env",
            "production",
            "--yes",
        ][..],
    ] {
        let output = run_tako_with_env(args, temp.path(), &home, &tako_home);
        assert!(!output.status.success(), "{args:?}");
        let value: serde_json::Value = serde_json::from_str(stdout_str(&output).trim()).unwrap();
        assert_eq!(value["ok"], false, "{args:?}");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("No network on this machine"),
            "{args:?}: {value}"
        );
    }
}

#[test]
fn workflows_refuse_the_development_environment() {
    let (temp, home, tako_home) = project();
    for action in [
        &["workflow", "list"][..],
        &["workflow", "show", "run_1"][..],
        &["workflow", "retry", "run_1"][..],
        &["workflow", "cancel", "run_1", "--yes"][..],
    ] {
        let mut args = vec!["--json"];
        args.extend_from_slice(action);
        args.extend(["--env", "development"]);
        let output = run_tako_with_env(&args, temp.path(), &home, &tako_home);
        assert!(!output.status.success(), "{args:?}");
        let value: serde_json::Value = serde_json::from_str(stdout_str(&output).trim()).unwrap();
        assert_eq!(
            value["error"]["message"],
            "Environment 'development' is reserved for local development. Workflow run commands only work against deployed environments.",
            "{args:?}"
        );
    }
}

#[test]
fn workflows_reject_unknown_environments_and_servers() {
    let (temp, home, tako_home) = project();
    let output = run_tako_with_env(
        &["--ci", "workflow", "list", "--env", "staging"],
        temp.path(),
        &home,
        &tako_home,
    );
    assert!(!output.status.success());
    assert!(
        stderr_str(&output).contains("Environment 'staging' not found in tako.toml."),
        "{}",
        stderr_str(&output)
    );

    let server = super::shared_state::ApiServer::new();
    let project = server.project("workflow-server-selection");
    let output = server.run(
        &server.owner_home,
        &project,
        &[
            "workflow",
            "show",
            "run_1",
            "--env",
            "production",
            "--server",
            "missing",
        ],
        None,
    );
    assert!(!output.status.success());
    assert!(
        stderr_str(&output).contains("Server 'missing' not found in enrolled network membership"),
        "{}",
        stderr_str(&output)
    );
}
