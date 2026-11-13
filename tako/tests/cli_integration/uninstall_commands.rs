use crate::support::*;

#[test]
fn uninstall_lists_targets_and_confirms() {
    // Non-interactive (stdin is null) — confirmation defaults to false,
    // so uninstall lists targets but does not actually delete anything.
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let tako_home = temp.path().join("tako-data");
    fs::create_dir_all(&tako_home).unwrap();
    fs::write(tako_home.join("config.toml"), "").unwrap();

    let output = run_tako_with_env(&["uninstall"], &project_dir, temp.path(), &tako_home);

    assert!(
        output.status.success(),
        "uninstall should succeed (cancelled): {}",
        stderr_str(&output)
    );
    let stderr = stderr_str(&output);
    // Should mention what will be removed
    assert!(
        stderr.contains("permanently remove"),
        "expected removal warning, got: {stderr}"
    );
    // Should show the TAKO_HOME path
    assert!(
        stderr.contains(&tako_home.display().to_string()),
        "expected TAKO_HOME in target list, got: {stderr}"
    );
    assert!(
        stderr.contains("Operation cancelled"),
        "expected shared cancellation message, got: {stderr}"
    );
    // Confirmation defaulted to no — nothing should be removed
    assert!(
        tako_home.exists(),
        "TAKO_HOME should still exist (confirmation was not given)"
    );
}

#[test]
fn servers_uninstall_without_name_in_non_interactive_mode_shows_hint() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    fs::write(
        tako_home.join("config.toml"),
        "[[servers]]\nname = 'real-srv'\nhost = '10.0.0.99'\n",
    )
    .unwrap();

    let uninstall = run_tako_with_env(
        &["network", "server", "uninstall"],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        !uninstall.status.success(),
        "uninstall without name should fail on non-tty"
    );

    let stderr = stderr_str(&uninstall);
    assert!(
        stderr.contains("requires an interactive terminal"),
        "expected helpful error for non-interactive uninstall: {stderr}"
    );
}

#[test]
fn servers_uninstall_dry_run_does_not_connect_or_modify_inventory() {
    let temp = TempDir::new().unwrap();
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&tako_home).unwrap();
    let config = "[[servers]]\nname = 'edge'\nhost = 'unreachable.invalid'\n";
    fs::write(tako_home.join("config.toml"), config).unwrap();
    let out = run_tako_with_env(
        &[
            "--dry-run",
            "network",
            "server",
            "uninstall",
            "edge",
            "--yes",
        ],
        temp.path(),
        temp.path(),
        &tako_home,
    );
    assert!(out.status.success(), "{}", stderr_str(&out));
    assert!(stderr_str(&out).contains("Remove tako-server and all data on edge"));
    assert_eq!(
        fs::read_to_string(tako_home.join("config.toml")).unwrap(),
        config
    );
}

#[test]
fn servers_uninstall_nonexistent_name_fails() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();

    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    fs::write(
        tako_home.join("config.toml"),
        "[[servers]]\nname = 'real-srv'\nhost = '10.0.0.99'\n",
    )
    .unwrap();

    let uninstall = run_tako_with_env(
        &["network", "server", "uninstall", "ghost-server", "--yes"],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        !uninstall.status.success(),
        "uninstall of nonexistent server should fail"
    );

    let stderr = stderr_str(&uninstall);
    assert!(
        stderr.contains("not found"),
        "expected 'not found' error, got: {stderr}"
    );
}
