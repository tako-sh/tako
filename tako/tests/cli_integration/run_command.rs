use crate::support::*;

fn write_fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

fn path_with_fake_bin(fake_bin: &Path) -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    format!("{}:{current}", fake_bin.display())
}

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
fn run_command_runs_bare_node_typescript_script_with_runtime_rule() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(project_dir.join("scripts")).unwrap();
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "demo"
runtime = "node"
"#,
    )
    .unwrap();
    fs::write(project_dir.join("scripts/foo.ts"), "console.log('ok');\n").unwrap();
    write_fake_bin(
        &fake_bin,
        "node",
        r#"#!/bin/sh
printf 'argv=%s\n' "$*"
printf 'env=%s\n' "$ENV"
printf 'bootstrap=%s\n' "$TAKO_BOOTSTRAP_DATA"
"#,
    );
    let path = path_with_fake_bin(&fake_bin);

    let output = run_tako_with_extra_env(
        &["run", "scripts/foo.ts", "--flag"],
        &project_dir,
        &[("PATH", path.as_str())],
    );

    assert!(
        output.status.success(),
        "tako run should succeed: {}",
        stderr_str(&output)
    );
    let out = stdout_str(&output);
    assert!(
        out.contains("argv=--experimental-strip-types scripts/foo.ts --flag"),
        "missing node script argv: {out}"
    );
    assert!(out.contains("env=development"), "missing ENV: {out}");
    assert!(
        out.contains("\"secrets\":{}"),
        "missing bootstrap envelope: {out}"
    );
}

#[test]
fn run_command_eval_runs_inline_code_with_runtime_rule() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "demo"
runtime = "node"
"#,
    )
    .unwrap();
    write_fake_bin(
        &fake_bin,
        "node",
        r#"#!/bin/sh
printf 'argv=%s\n' "$*"
for arg in "$@"; do
  if [ -f "$arg" ]; then
    printf 'body=%s\n' "$(cat "$arg")"
  fi
done
"#,
    );
    let path = path_with_fake_bin(&fake_bin);

    let output = run_tako_with_extra_env(
        &["run", "--eval", "console.log('inline')", "--", "--flag"],
        &project_dir,
        &[("PATH", path.as_str())],
    );

    assert!(
        output.status.success(),
        "tako run --eval should succeed: {}",
        stderr_str(&output)
    );
    let out = stdout_str(&output);
    assert!(
        out.contains("argv=--experimental-strip-types"),
        "missing node eval argv: {out}"
    );
    assert!(out.contains(".ts --flag"), "missing eval arg: {out}");
    assert!(
        out.contains("body=console.log('inline')"),
        "missing inline body: {out}"
    );
}

#[test]
fn run_command_runs_bare_go_script_with_go_run() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(project_dir.join("jobs")).unwrap();
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "demo"
runtime = "go"
"#,
    )
    .unwrap();
    fs::write(project_dir.join("jobs/foo.go"), "package main\n").unwrap();
    write_fake_bin(
        &fake_bin,
        "go",
        r#"#!/bin/sh
printf 'argv=%s\n' "$*"
"#,
    );
    let path = path_with_fake_bin(&fake_bin);

    let output = run_tako_with_extra_env(
        &["run", "jobs/foo.go", "--flag"],
        &project_dir,
        &[("PATH", path.as_str())],
    );

    assert!(
        output.status.success(),
        "tako run should succeed: {}",
        stderr_str(&output)
    );
    let out = stdout_str(&output);
    assert!(
        out.contains("argv=run jobs/foo.go --flag"),
        "missing go run argv: {out}"
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
fn run_command_loads_encrypted_secrets_into_bootstrap() {
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
            "--",
            "sh",
            "-c",
            "printf 'bootstrap=%s\\ndata=%s\\n' \"$TAKO_BOOTSTRAP_DATA\" \"$TAKO_DATA_DIR\"",
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
        out.contains("\"API_KEY\":\"secret-value\""),
        "missing bootstrap secret: {out}"
    );
    assert!(
        out.contains(".tako/data/app"),
        "missing TAKO_DATA_DIR: {out}"
    );
}
