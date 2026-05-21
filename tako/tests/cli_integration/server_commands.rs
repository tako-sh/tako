use crate::support::*;

#[test]
fn test_server_list_empty() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    // Create empty config.toml
    let tako_dir = project_dir.join(".tako");
    fs::create_dir_all(&tako_dir).unwrap();
    fs::write(tako_dir.join("config.toml"), "").unwrap();

    // Point tako at this isolated TAKO_HOME.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(["servers", "list"])
        .current_dir(&project_dir)
        .env("HOME", &project_dir)
        .env("TAKO_HOME", &tako_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    let output = cmd.output().expect("Failed to run tako command");

    assert!(
        output.status.success(),
        "tako servers list failed: {}",
        stderr_str(&output)
    );

    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        combined.contains("No servers configured"),
        "Should show no servers warning: {}",
        combined
    );
    assert!(
        combined.contains("Run 'tako servers add' to add a server.")
            || combined.contains("Run tako servers add to add a server."),
        "Should include add-server hint: {}",
        combined
    );
    assert!(
        !combined.contains("Add one now?"),
        "servers list should not launch an add wizard: {}",
        combined
    );
}

#[test]
fn servers_add_creates_missing_tako_home() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    // Point HOME somewhere safe and set TAKO_HOME to a missing directory.
    let home = temp.path().join("home");
    let tako_home = temp.path().join("missing-tako-home");
    fs::create_dir_all(&home).unwrap();
    assert!(!tako_home.exists());

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako"));
    cmd.args(["servers", "add", "1.2.3.4", "--name", "prod", "--no-test"])
        .current_dir(&project_dir)
        .env("HOME", &home)
        .env("TAKO_HOME", &tako_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_coverage_env(&mut cmd);
    let output = cmd.output().expect("Failed to run tako command");

    assert!(
        output.status.success(),
        "tako servers add failed: {}{}",
        stdout_str(&output),
        stderr_str(&output)
    );

    assert!(tako_home.join("config.toml").exists());
}

#[test]
fn servers_add_with_hostname_derives_name() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let out = run_tako_with_env(
        &["servers", "add", "my-server", "--no-test"],
        &project_dir,
        &home,
        &tako_home,
    );

    assert!(
        out.status.success(),
        "servers add should derive a name from host: {}{}",
        stdout_str(&out),
        stderr_str(&out)
    );

    let config = fs::read_to_string(tako_home.join("config.toml")).unwrap();
    assert!(config.contains("name = \"my-server\""), "{config}");
    assert!(config.contains("host = \"my-server\""), "{config}");
}

#[test]
fn servers_add_with_magicdns_host_derives_short_name() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let out = run_tako_with_env(
        &["servers", "add", "my-server.tailnet.ts.net", "--no-test"],
        &project_dir,
        &home,
        &tako_home,
    );

    assert!(
        out.status.success(),
        "servers add should derive a short name from MagicDNS: {}{}",
        stdout_str(&out),
        stderr_str(&out)
    );

    let config = fs::read_to_string(tako_home.join("config.toml")).unwrap();
    assert!(config.contains("name = \"my-server\""), "{config}");
    assert!(
        config.contains("host = \"my-server.tailnet.ts.net\""),
        "{config}"
    );
}

#[test]
fn servers_add_accepts_admin_user_host_shorthand() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let out = run_tako_with_env(
        &["servers", "add", "ubuntu@my-server", "--no-test"],
        &project_dir,
        &home,
        &tako_home,
    );

    assert!(
        out.status.success(),
        "servers add should accept admin-user@host: {}{}",
        stdout_str(&out),
        stderr_str(&out)
    );

    let config = fs::read_to_string(tako_home.join("config.toml")).unwrap();
    assert!(config.contains("name = \"my-server\""), "{config}");
    assert!(config.contains("host = \"my-server\""), "{config}");
}

#[test]
fn servers_add_with_ip_requires_name() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let out = run_tako_with_env(
        &["servers", "add", "1.2.3.4", "--no-test"],
        &project_dir,
        &home,
        &tako_home,
    );

    assert!(
        !out.status.success(),
        "servers add without --name should fail: {}{}",
        stdout_str(&out),
        stderr_str(&out)
    );

    let combined = format!("{}{}", stdout_str(&out), stderr_str(&out));
    assert!(
        combined.contains("Server name is required"),
        "expected missing-name guidance: {}",
        combined
    );
}

#[test]
fn servers_add_rejects_non_tailscale_host_before_writing_config() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let out = run_tako_with_env(
        &["servers", "add", "127.0.0.1", "--name", "local"],
        &project_dir,
        &home,
        &tako_home,
    );

    assert!(
        !out.status.success(),
        "servers add should reject non-Tailscale hosts: {}{}",
        stdout_str(&out),
        stderr_str(&out)
    );

    let combined = format!("{}{}", stdout_str(&out), stderr_str(&out));
    assert!(
        combined.contains("Remote management requires Tailscale"),
        "expected Tailscale guidance: {}",
        combined
    );

    let config = fs::read_to_string(tako_home.join("config.toml")).unwrap_or_default();
    assert!(
        !config.contains("[[servers]]"),
        "server should not be written after access failure: {}",
        config
    );
}

#[test]
fn servers_add_persists_description() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let out = run_tako_with_env(
        &[
            "servers",
            "add",
            "10.0.0.1",
            "--name",
            "edge",
            "--description",
            "Edge POP",
            "--no-test",
        ],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        out.status.success(),
        "add with description should succeed: {}{}",
        stdout_str(&out),
        stderr_str(&out)
    );

    let servers_toml = fs::read_to_string(tako_home.join("config.toml")).unwrap();
    assert!(
        servers_toml.contains("description = \"Edge POP\""),
        "config.toml should include description: {}",
        servers_toml
    );
}

#[test]
fn servers_list_shows_description_column() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let add = run_tako_with_env(
        &[
            "servers",
            "add",
            "10.0.0.2",
            "--name",
            "eu-edge",
            "--description",
            "EU Edge",
            "--no-test",
        ],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(add.status.success(), "add should succeed");

    let ls = run_tako_with_env(&["servers", "list"], &project_dir, &home, &tako_home);
    assert!(
        ls.status.success(),
        "servers list should succeed: {}{}",
        stdout_str(&ls),
        stderr_str(&ls)
    );

    let out = stderr_str(&ls);
    assert!(
        out.contains("Description"),
        "expected description field: {}",
        out
    );
    assert!(
        out.contains("EU Edge"),
        "expected description value: {}",
        out
    );
}

#[test]
fn servers_remove_without_name_in_non_interactive_mode_shows_hint() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let add = run_tako_with_env(
        &[
            "servers",
            "add",
            "10.0.0.3",
            "--name",
            "prod-1",
            "--no-test",
        ],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        add.status.success(),
        "add should succeed: {}{}",
        stdout_str(&add),
        stderr_str(&add)
    );

    let rm = run_tako_with_env(&["servers", "remove"], &project_dir, &home, &tako_home);
    assert!(
        !rm.status.success(),
        "remove without name should fail on non-tty"
    );

    let stderr = stderr_str(&rm);
    assert!(
        stderr.contains("requires an interactive terminal")
            || stderr.contains("provide a server name"),
        "expected helpful error for non-interactive remove without name: {}",
        stderr
    );
}

#[test]
fn servers_remove_named_non_interactive_uses_operation_cancelled_message() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let add = run_tako_with_env(
        &[
            "servers",
            "add",
            "10.0.0.4",
            "--name",
            "prod-2",
            "--no-test",
        ],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        add.status.success(),
        "add should succeed: {}{}",
        stdout_str(&add),
        stderr_str(&add)
    );

    let rm = run_tako_with_env(
        &["servers", "remove", "prod-2"],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        rm.status.success(),
        "rm cancellation should preserve current success behavior: {}{}",
        stdout_str(&rm),
        stderr_str(&rm)
    );

    let stderr = stderr_str(&rm);
    assert!(
        stderr.contains("Operation cancelled"),
        "expected shared cancellation message: {stderr}"
    );

    let ls = run_tako_with_env(&["servers", "list"], &project_dir, &home, &tako_home);
    assert!(
        ls.status.success(),
        "servers list should succeed after cancellation: {}{}",
        stdout_str(&ls),
        stderr_str(&ls)
    );
    let servers_output = stderr_str(&ls);
    assert!(
        servers_output.contains("prod-2"),
        "server should remain after cancellation: {servers_output}"
    );
}

#[test]
fn servers_add_is_idempotent_for_same_name_host_and_port() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let first = run_tako_with_env(
        &[
            "servers",
            "add",
            "127.0.0.1",
            "--name",
            "prod",
            "--port",
            "2222",
            "--no-test",
        ],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        first.status.success(),
        "first add should succeed: {}{}",
        stdout_str(&first),
        stderr_str(&first)
    );

    let second = run_tako_with_env(
        &[
            "servers",
            "add",
            "127.0.0.1",
            "--name",
            "prod",
            "--port",
            "2222",
        ],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        second.status.success(),
        "second add should be idempotent: {}{}",
        stdout_str(&second),
        stderr_str(&second)
    );

    let combined = format!("{}{}", stdout_str(&second), stderr_str(&second));
    assert!(
        combined.contains("already configured"),
        "expected idempotent message: {}",
        combined
    );
}

#[test]
fn servers_add_records_cli_history_for_autocomplete() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let add = run_tako_with_env(
        &[
            "servers",
            "add",
            "203.0.113.5",
            "--name",
            "edge-us",
            "--port",
            "2201",
            "--no-test",
        ],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        add.status.success(),
        "add should succeed: {}{}",
        stdout_str(&add),
        stderr_str(&add)
    );

    let history_path = tako_home.join("history.toml");
    let history_raw = fs::read_to_string(&history_path).expect("history file should exist");
    assert!(
        history_raw.contains("203.0.113.5"),
        "history should include host: {}",
        history_raw
    );
    assert!(
        history_raw.contains("edge-us"),
        "history should include server name: {}",
        history_raw
    );
    assert!(
        history_raw.contains("2201"),
        "history should include port: {}",
        history_raw
    );
    assert!(
        !history_raw.contains("[[servers]]"),
        "history should be separate from server config: {}",
        history_raw
    );
}
