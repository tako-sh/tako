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
    cmd.args(["network", "server", "list"])
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
        "tako network server list failed: {}",
        stderr_str(&output)
    );

    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        combined.contains("No servers configured"),
        "Should show no servers warning: {}",
        combined
    );
    assert!(
        combined.contains("Run 'tako network server add' to add a server.")
            || combined.contains("Run tako network server add to add a server."),
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
fn servers_add_requires_native_network_without_writing_inventory() {
    let temp = TempDir::new().unwrap();
    let tako_home = temp.path().join("tako-home");
    let out = run_tako_with_env(
        &["--ci", "network", "server", "add", "admin@192.0.2.1"],
        temp.path(),
        temp.path(),
        &tako_home,
    );
    assert!(!out.status.success());
    assert!(stderr_str(&out).contains("No network configured"));
    assert!(!tako_home.join("networks/identities.json").exists());
}

#[test]
fn native_server_add_dry_run_uses_sole_network_without_touching_server_or_inventory() {
    let temp = TempDir::new().unwrap();
    let tako_home = temp.path().join("tako-home");
    let created = run_tako_with_env(
        &["network", "create", "fleet"],
        temp.path(),
        temp.path(),
        &tako_home,
    );
    assert!(created.status.success(), "{}", stderr_str(&created));
    let before = fs::read(tako_home.join("networks/identities.json")).unwrap();
    let out = run_tako_with_env(
        &[
            "--dry-run",
            "--json",
            "network",
            "server",
            "add",
            "admin@not-resolved.invalid",
            "--name",
            "edge",
            "--http-port",
            "8080",
            "--https-port",
            "8443",
        ],
        temp.path(),
        temp.path(),
        &tako_home,
    );
    assert!(out.status.success(), "{}", stderr_str(&out));
    let data: serde_json::Value = serde_json::from_str(&stdout_str(&out)).unwrap();
    assert_eq!(data["network"], "fleet");
    assert_eq!(data["operator"], "admin");
    assert_eq!(data["server"], "edge");
    assert_eq!(
        fs::read(tako_home.join("networks/identities.json")).unwrap(),
        before
    );
}

#[test]
fn legacy_inventory_does_not_grant_server_membership() {
    let temp = TempDir::new().unwrap();
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&tako_home).unwrap();
    let config = "[[servers]]\nname='old'\nhost='192.0.2.1'\nvpn='netbird'\n";
    fs::write(tako_home.join("config.toml"), config).unwrap();
    let out = run_tako_with_env(
        &["--json", "network", "server", "list"],
        temp.path(),
        temp.path(),
        &tako_home,
    );
    assert!(out.status.success(), "{}", stderr_str(&out));
    assert!(!stdout_str(&out).contains("192.0.2.1"));
    assert_eq!(
        fs::read_to_string(tako_home.join("config.toml")).unwrap(),
        config
    );
}

#[test]
fn network_commands_use_explicit_then_chosen_then_sole_network() {
    let temp = TempDir::new().unwrap();
    let tako_home = temp.path().join("tako-home");
    let run = |args: &[&str]| run_tako_with_env(args, temp.path(), temp.path(), &tako_home);
    let add = |extra: &[&str]| {
        let mut args = vec![
            "--dry-run",
            "--json",
            "network",
            "server",
            "add",
            "admin@unreachable.invalid",
            "--name",
            "edge",
        ];
        args.extend_from_slice(extra);
        run(&args)
    };
    let network_of = |out: std::process::Output| {
        assert!(out.status.success(), "{}", stderr_str(&out));
        serde_json::from_str::<serde_json::Value>(&stdout_str(&out)).unwrap()["network"].clone()
    };

    assert!(run(&["network", "create", "default"]).status.success());
    // A sole network is selected without naming it.
    assert_eq!(network_of(add(&[])), "default");

    assert!(run(&["network", "create", "selected"]).status.success());
    let listed: serde_json::Value =
        serde_json::from_slice(&run(&["--json", "network", "list"]).stdout).unwrap();
    let selected_id = listed["networks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|network| network["name"] == "selected")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A project's production network no longer selects one implicitly.
    fs::write(
        temp.path().join("tako.toml"),
        format!(
            "name = 'test-app'\n[envs.production]\nroute = 'test.example.com'\nnetwork = '{selected_id}'\n"
        ),
    )
    .unwrap();
    let ambiguous = add(&[]);
    assert!(!ambiguous.status.success());
    assert!(
        stderr_str(&ambiguous).contains("tako network use"),
        "{}",
        stderr_str(&ambiguous)
    );
    assert!(
        listed["networks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|network| network["default"] == false)
    );

    let inventory_before = fs::read(tako_home.join("networks/identities.json")).unwrap();
    let used = run(&["--json", "network", "use", "selected"]);
    assert!(used.status.success(), "{}", stderr_str(&used));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&used.stdout).unwrap()["id"],
        selected_id.as_str()
    );
    assert_eq!(network_of(add(&[])), "selected");
    // Explicit arguments always override the chosen network.
    assert_eq!(network_of(add(&["--network", "default"])), "default");
    let listed: serde_json::Value =
        serde_json::from_slice(&run(&["--json", "network", "list"]).stdout).unwrap();
    for network in listed["networks"].as_array().unwrap() {
        assert_eq!(network["default"], network["name"] == "selected");
    }
    assert!(!run(&["network", "use", "missing"]).status.success());
    assert_eq!(
        fs::read(tako_home.join("networks/identities.json")).unwrap(),
        inventory_before
    );
}

#[test]
fn bare_server_names_resolve_in_the_selected_network() {
    let fixture = super::native_management::Fixture::with_servers(&["edge"]);
    let run = |args: &[&str]| {
        run_tako_with_env(
            args,
            fixture.directory.path(),
            &fixture.home,
            &fixture.tako_home,
        )
    };
    let bare = run(&["--ci", "--dry-run", "network", "server", "revoke", "edge"]);
    assert!(bare.status.success(), "{}", stderr_str(&bare));
    assert!(
        stderr_str(&bare).contains("fleet/edge"),
        "{}",
        stderr_str(&bare)
    );
    let explicit = run(&[
        "--ci",
        "--dry-run",
        "network",
        "server",
        "revoke",
        "edge",
        "--network",
        "fleet",
    ]);
    assert!(explicit.status.success(), "{}", stderr_str(&explicit));
    let unknown = run(&[
        "--ci",
        "--dry-run",
        "network",
        "server",
        "revoke",
        "edge",
        "--network",
        "missing",
    ]);
    assert!(!unknown.status.success());
    assert!(
        stderr_str(&unknown).contains("not configured"),
        "{}",
        stderr_str(&unknown)
    );
}

#[test]
fn servers_list_reads_enrolled_names_and_endpoints() {
    let fixture = super::native_management::Fixture::with_servers(&["eu-edge", "us-edge"]);
    let out = run_tako_with_env(
        &["--json", "network", "server", "list"],
        fixture.directory.path(),
        &fixture.home,
        &fixture.tako_home,
    );
    assert!(out.status.success(), "{}", stderr_str(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let servers = json["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 2);
    let output = stdout_str(&out);
    assert!(output.contains("fleet/eu-edge"));
    assert!(output.contains("fleet/us-edge"));
}

#[test]
fn server_revoke_requires_selection_and_preserves_membership_when_cancelled() {
    let fixture = super::native_management::Fixture::with_servers(&["edge"]);
    let before = fs::read(&fixture.profile_path).unwrap();
    let missing = run_tako_with_env(
        &["network", "server", "revoke"],
        fixture.directory.path(),
        &fixture.home,
        &fixture.tako_home,
    );
    assert!(!missing.status.success());
    assert!(stderr_str(&missing).contains("interactive"));
    let cancelled = run_tako_with_env(
        &["network", "server", "revoke", "fleet/edge"],
        fixture.directory.path(),
        &fixture.home,
        &fixture.tako_home,
    );
    assert!(cancelled.status.success(), "{}", stderr_str(&cancelled));
    assert!(stderr_str(&cancelled).contains("Operation cancelled"));
    assert_eq!(fs::read(&fixture.profile_path).unwrap(), before);
}

#[test]
fn server_remove_refuses_the_last_server_without_yes_and_dry_run_preserves_membership() {
    let fixture = super::native_management::Fixture::with_servers(&["edge"]);
    let before = fs::read(&fixture.profile_path).unwrap();
    let run = |args: &[&str]| {
        run_tako_with_env(
            args,
            fixture.directory.path(),
            &fixture.home,
            &fixture.tako_home,
        )
    };
    let missing = run(&["network", "server", "remove"]);
    assert!(!missing.status.success());
    assert!(stderr_str(&missing).contains("interactive"));
    let unknown = run(&["network", "server", "rm", "ghost", "--yes"]);
    assert!(!unknown.status.success());
    assert!(stderr_str(&unknown).contains("not found"));
    let last = run(&["--dry-run", "network", "server", "remove", "fleet/edge"]);
    assert!(!last.status.success());
    let message = stderr_str(&last);
    assert!(
        message.contains("last server in network fleet") && message.contains("--yes"),
        "{message}"
    );
    let dry = run(&[
        "--dry-run",
        "--json",
        "network",
        "server",
        "remove",
        "fleet/edge",
        "--uninstall",
        "--yes",
    ]);
    assert!(dry.status.success(), "{}", stderr_str(&dry));
    let report: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["network"], "fleet");
    assert_eq!(report["server"], "fleet/edge");
    assert_eq!(report["uninstalled"], true);
    let preview = run(&["--dry-run", "network", "server", "remove", "edge", "--yes"]);
    assert!(preview.status.success(), "{}", stderr_str(&preview));
    assert!(
        stderr_str(&preview).contains("Delete every app and its data on fleet/edge"),
        "{}",
        stderr_str(&preview)
    );
    assert!(!stderr_str(&preview).contains("Remove tako-server"));
    assert_eq!(fs::read(&fixture.profile_path).unwrap(), before);
}
