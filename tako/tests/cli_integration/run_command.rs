use crate::support::*;

#[test]
fn run_command_passes_tako_context_to_child() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "demo"
runtime = "bun"

[vars]
API_URL = "https://api.example.com"

[vars.staging]
API_URL = "https://staging.example.com"
"#,
    )
    .unwrap();

    let output = run_tako(
        &[
            "run",
            "--env",
            "staging",
            "--",
            "sh",
            "-c",
            "printf 'env=%s\\napi=%s\\nnode=%s\\nbootstrap=%s\\n' \"$ENV\" \"$API_URL\" \"$NODE_ENV\" \"$TAKO_BOOTSTRAP_DATA\"",
        ],
        &project_dir,
    );

    assert!(
        output.status.success(),
        "tako run should succeed: {}",
        stderr_str(&output)
    );

    let out = stdout_str(&output);
    assert!(out.contains("env=staging"), "missing ENV: {out}");
    assert!(
        out.contains("api=https://staging.example.com"),
        "missing env-specific var: {out}"
    );
    assert!(
        out.contains("node=production"),
        "missing runtime env: {out}"
    );
    assert!(
        out.contains("\"secrets\":{}"),
        "missing bootstrap envelope: {out}"
    );
}

#[test]
fn run_command_propagates_child_failure_without_error_copy() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    fs::write(project_dir.join("tako.toml"), "name = \"demo\"\n").unwrap();

    let output = run_tako(&["run", "--", "sh", "-c", "exit 7"], &project_dir);

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(stderr_str(&output), "");
}

#[test]
fn run_command_loads_encrypted_secrets_into_bootstrap_and_optional_env() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&project_dir).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();
    fs::write(project_dir.join("tako.toml"), "name = \"demo\"\n").unwrap();

    let set = run_tako_with_stdin_and_env(
        &["secrets", "set", "API_KEY", "--env", "development"],
        &project_dir,
        "secret-value\n",
        &home,
        &tako_home,
    );
    assert!(
        set.status.success(),
        "secret set should succeed: {}",
        stderr_str(&set)
    );

    let output = run_tako_with_env(
        &[
            "run",
            "--secrets-as-env",
            "--",
            "sh",
            "-c",
            "printf 'secret_env=%s\\nbootstrap=%s\\ndata=%s\\n' \"$API_KEY\" \"$TAKO_BOOTSTRAP_DATA\" \"$TAKO_DATA_DIR\"",
        ],
        &project_dir,
        &home,
        &tako_home,
    );

    assert!(
        output.status.success(),
        "tako run should succeed: {}",
        stderr_str(&output)
    );
    let out = stdout_str(&output);
    assert!(
        out.contains("secret_env=secret-value"),
        "missing secret env: {out}"
    );
    assert!(
        out.contains("\"API_KEY\":\"secret-value\""),
        "missing bootstrap secret: {out}"
    );
    assert!(
        out.contains(".tako/data/app"),
        "missing TAKO_DATA_DIR: {out}"
    );
}
