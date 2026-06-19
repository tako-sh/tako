use crate::support::*;

#[test]
fn ci_flag_disables_ansi_colors() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path();

    // Create a minimal project that will produce some output
    let tako_dir = project_dir.join(".tako");
    fs::create_dir_all(&tako_dir).unwrap();
    fs::write(tako_dir.join("config.toml"), "").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(["--ci", "servers", "list"])
        .current_dir(project_dir)
        .env("HOME", project_dir)
        .env("TAKO_HOME", &tako_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    let output = cmd.output().expect("Failed to run tako command");

    assert!(output.status.success());

    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    // CI mode should have no ANSI escape sequences
    assert!(
        !combined.contains("\x1b["),
        "CI mode should not contain ANSI escape codes: {combined}"
    );
}

#[test]
fn verbose_flag_produces_timestamped_output() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path();

    let tako_dir = project_dir.join(".tako");
    fs::create_dir_all(&tako_dir).unwrap();
    fs::write(tako_dir.join("config.toml"), "").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(["--verbose", "servers", "list"])
        .current_dir(project_dir)
        .env("HOME", project_dir)
        .env("TAKO_HOME", &tako_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    let output = cmd.output().expect("Failed to run tako command");

    assert!(output.status.success());

    let err = stderr_str(&output);
    // Verbose output should contain timestamp-prefixed lines (HH:MM:SS)
    let has_timestamp = err.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.len() >= 8 && trimmed.as_bytes()[2] == b':' && trimmed.as_bytes()[5] == b':'
    });
    assert!(
        has_timestamp,
        "Verbose mode should produce timestamped log lines on stderr: {err}"
    );
}

#[test]
fn ci_and_verbose_combined() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path();

    let tako_dir = project_dir.join(".tako");
    fs::create_dir_all(&tako_dir).unwrap();
    fs::write(tako_dir.join("config.toml"), "").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(["--ci", "--verbose", "servers", "list"])
        .current_dir(project_dir)
        .env("HOME", project_dir)
        .env("TAKO_HOME", &tako_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    let output = cmd.output().expect("Failed to run tako command");

    assert!(output.status.success());

    let err = stderr_str(&output);
    // CI mode skips timestamps (CI systems add their own) and ANSI codes
    let has_timestamp = err.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.len() >= 8 && trimmed.as_bytes()[2] == b':' && trimmed.as_bytes()[5] == b':'
    });
    assert!(
        !has_timestamp,
        "CI mode should not produce timestamps (CI systems add their own): {err}"
    );
    assert!(
        !err.contains("\x1b["),
        "CI+verbose should not contain ANSI codes: {err}"
    );
}

#[test]
fn verbose_output_goes_to_stderr() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path();

    let tako_dir = project_dir.join(".tako");
    fs::create_dir_all(&tako_dir).unwrap();
    fs::write(tako_dir.join("config.toml"), "").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(["--verbose", "servers", "list"])
        .current_dir(project_dir)
        .env("HOME", project_dir)
        .env("TAKO_HOME", &tako_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    let output = cmd.output().expect("Failed to run tako command");

    assert!(output.status.success());

    let err = stderr_str(&output);
    // Verbose log lines should be on stderr, not stdout
    assert!(!err.is_empty(), "Verbose output should appear on stderr");
}

#[test]
fn json_version_outputs_structured_stdout() {
    let temp = TempDir::new().unwrap();
    let output = run_tako(&["--json", "version"], temp.path());

    assert!(output.status.success());
    assert!(
        stderr_str(&output).is_empty(),
        "version JSON should not write progress: {}",
        stderr_str(&output)
    );

    let value: serde_json::Value = serde_json::from_str(stdout_str(&output).trim()).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "version");
    assert!(value["version"].as_str().is_some());
}

#[test]
fn json_status_without_servers_outputs_empty_server_list() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path();
    let tako_home = project_dir.join(".tako-home");
    fs::create_dir_all(&tako_home).unwrap();
    fs::write(tako_home.join("config.toml"), "").unwrap();

    let output = run_tako_with_env(&["--json", "status"], project_dir, project_dir, &tako_home);

    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_str(stdout_str(&output).trim()).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "status");
    assert_eq!(value["servers"].as_array().unwrap().len(), 0);
}

#[test]
fn json_failures_output_structured_error_stdout() {
    let temp = TempDir::new().unwrap();
    let output = run_tako(&["--json", "deploy"], temp.path());

    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_str(stdout_str(&output).trim()).unwrap();
    assert_eq!(value["ok"], false);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tako.toml")
    );
    assert!(
        stderr_str(&output).contains("ERROR"),
        "human-readable error should still go to stderr: {}",
        stderr_str(&output)
    );
}
