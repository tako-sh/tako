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
fn network_server_uninstall_requires_a_known_target_and_dry_run_preserves_membership() {
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
    let missing = run(&["network", "server", "uninstall"]);
    assert!(!missing.status.success());
    assert!(stderr_str(&missing).contains("interactive"));
    let unknown = run(&["network", "server", "uninstall", "ghost", "--yes"]);
    assert!(!unknown.status.success());
    assert!(stderr_str(&unknown).contains("not found"));
    let dry = run(&[
        "--dry-run",
        "network",
        "server",
        "uninstall",
        "fleet/edge",
        "--yes",
    ]);
    assert!(dry.status.success(), "{}", stderr_str(&dry));
    assert!(stderr_str(&dry).contains("Remove tako-server and all data on fleet/edge"));
    assert_eq!(fs::read(&fixture.profile_path).unwrap(), before);
}
