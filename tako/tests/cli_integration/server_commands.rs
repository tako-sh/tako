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
    assert!(!tako_home.join("config.toml").exists());
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
    let before = fs::read(tako_home.join("config.toml")).unwrap();
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
    assert_eq!(fs::read(tako_home.join("config.toml")).unwrap(), before);
}

#[test]
fn removed_vpn_configuration_is_not_silently_accepted() {
    let temp = TempDir::new().unwrap();
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&tako_home).unwrap();
    for provider in ["tailscale", "netbird"] {
        let config =
            format!("[[servers]]\nname = 'edge'\nhost = 'edge.internal'\nvpn = '{provider}'\n");
        fs::write(tako_home.join("config.toml"), &config).unwrap();
        let out = run_tako_with_env(
            &["network", "server", "list", "--json"],
            temp.path(),
            temp.path(),
            &tako_home,
        );
        assert!(!out.status.success());
        assert!(stderr_str(&out).contains("removed VPN provider"));
        assert_eq!(
            fs::read_to_string(tako_home.join("config.toml")).unwrap(),
            config
        );
    }
}

#[test]
fn native_server_add_honors_selected_project_config_and_explicit_network() {
    let temp = TempDir::new().unwrap();
    let tako_home = temp.path().join("tako-home");
    for alias in ["default", "selected"] {
        let out = run_tako_with_env(
            &["network", "create", alias],
            temp.path(),
            temp.path(),
            &tako_home,
        );
        assert!(out.status.success(), "{}", stderr_str(&out));
    }
    let inventory_before = fs::read(tako_home.join("config.toml")).unwrap();
    let config: toml::Value =
        toml::from_str(std::str::from_utf8(&inventory_before).unwrap()).unwrap();
    for (alias, file) in [("default", "tako.toml"), ("selected", "other.toml")] {
        let network = config["networks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|network| network["name"].as_str() == Some(alias))
            .unwrap();
        fs::write(
            temp.path().join(file),
            format!(
                "name = 'test-app'\n[envs.production]\nroute = 'test.example.com'\nnetwork = '{}'\n",
                network["id"].as_str().unwrap(),
            ),
        )
        .unwrap();
    }
    let add = |extra: &[&str]| {
        let mut args = vec![
            "--dry-run",
            "--json",
            "--config",
            "other.toml",
            "network",
            "server",
            "add",
            "admin@unreachable.invalid",
            "--name",
            "edge",
        ];
        args.extend_from_slice(extra);
        let out = run_tako_with_env(&args, temp.path(), temp.path(), &tako_home);
        assert!(out.status.success(), "{}", stderr_str(&out));
        serde_json::from_str::<serde_json::Value>(&stdout_str(&out)).unwrap()
    };
    assert_eq!(add(&[])["network"], "selected");
    assert_eq!(add(&["--network", "default"])["network"], "default");
    fs::remove_file(temp.path().join("tako.toml")).unwrap();
    assert_eq!(add(&[])["network"], "selected");
    assert_eq!(
        fs::read(tako_home.join("config.toml")).unwrap(),
        inventory_before
    );
}

#[test]
fn servers_list_renders_aligned_name_host_and_description_columns() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    fs::write(tako_home.join("config.toml"), "[[servers]]\nname = 'eu-edge'\nhost = '10.0.0.2'\ndescription = 'EU Edge'\n[[servers]]\nname = 'us-edge'\nhost = '東京'\ndescription = 'US Edge'\n").unwrap();

    let ls = run_tako_with_env(
        &["network", "server", "list"],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        ls.status.success(),
        "servers list should succeed: {}{}",
        stdout_str(&ls),
        stderr_str(&ls)
    );

    let out = stderr_str(&ls);
    assert!(out.contains("NAME"), "expected name column: {}", out);
    assert!(out.contains("HOST"), "expected host column: {}", out);
    assert!(
        out.contains("DESCRIPTION"),
        "expected description column: {}",
        out
    );
    assert!(
        out.contains("eu-edge  10.0.0.2  EU Edge"),
        "expected aligned EU Edge row: {}",
        out
    );
    assert!(
        out.contains("us-edge  東京      US Edge"),
        "expected aligned US Edge row: {}",
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

    fs::write(
        tako_home.join("config.toml"),
        "[[servers]]\nname = 'prod-1'\nhost = '10.0.0.4'\n",
    )
    .unwrap();

    let rm = run_tako_with_env(
        &["network", "server", "remove"],
        &project_dir,
        &home,
        &tako_home,
    );
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

    fs::write(
        tako_home.join("config.toml"),
        "[[servers]]\nname = 'prod-2'\nhost = '10.0.0.4'\n",
    )
    .unwrap();

    let rm = run_tako_with_env(
        &["network", "server", "remove", "prod-2"],
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

    let ls = run_tako_with_env(
        &["network", "server", "list"],
        &project_dir,
        &home,
        &tako_home,
    );
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
