use crate::support::*;

#[test]
fn test_status_shows_app_info() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    // Create tako.toml with proper env section
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "my-test-app"
runtime = "bun"
main = "index.ts"

[envs.production]
route = "prod.example.com"
"#,
    )
    .unwrap();

    let output = run_tako_with_env(&["status"], &project_dir, &home, &tako_home);

    // Status should show discovered app info, a summary header, or an empty inventory message.
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        combined.contains("my-test-app")
            || combined.contains("production")
            || combined.contains("Status")
            || combined.contains("No servers configured.")
            || combined.contains("No servers"),
        "Should show app info or status: {}",
        combined
    );
}

#[test]
fn test_status_without_tako_toml() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    let output = run_tako_with_env(&["status"], &project_dir, &home, &tako_home);

    // Status should work without project config and use global server inventory.
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        output.status.success(),
        "status should not require tako.toml: {}",
        combined
    );
    assert!(
        combined.contains("No servers")
            || combined.contains("Add one now")
            || combined.contains("No deployed apps"),
        "should report global status context when no servers/apps: {}",
        combined
    );
}

#[test]
fn test_status_with_server_name_is_rejected() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    let output = run_tako(&["status", "tako-server"], &project_dir);

    assert!(
        !output.status.success(),
        "status with server name should be rejected"
    );

    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        combined.contains("unexpected argument 'tako-server'")
            || combined.contains("Usage: tako status"),
        "should show parse usage error: {}",
        combined
    );
}
