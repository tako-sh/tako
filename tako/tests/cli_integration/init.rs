use crate::support::*;

#[test]
fn test_init_creates_tako_toml() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("test-app");
    fs::create_dir_all(&project_dir).unwrap();

    // Create a minimal package.json
    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "test-app", "version": "1.0.0"}"#,
    )
    .unwrap();

    // Create entry point
    fs::write(
        project_dir.join("index.ts"),
        r#"export default { fetch() { return new Response("ok"); } };"#,
    )
    .unwrap();

    // Run tako init (non-interactive: no existing tako.toml, so no confirmation needed)
    let output = run_tako(&["init"], &project_dir);

    assert!(
        output.status.success(),
        "tako init failed: {}",
        stderr_str(&output)
    );

    // Check tako.toml was created
    let tako_toml = project_dir.join("tako.toml");
    assert!(tako_toml.exists(), "tako.toml should be created");

    let content = fs::read_to_string(&tako_toml).unwrap();
    // The generated format uses required top-level app metadata fields.
    assert!(
        content.contains("name = \"test-app\""),
        "tako.toml should have required top-level name: {}",
        content
    );
    assert!(
        !content.contains("# name = \"test-app\""),
        "tako.toml should not leave name commented: {}",
        content
    );
}

#[test]
fn test_init_accepts_config_flag_for_subdirectory() {
    let temp = TempDir::new().unwrap();
    let root_dir = temp.path().to_path_buf();
    let project_dir = root_dir.join("my-app");
    fs::create_dir_all(&project_dir).unwrap();

    // Create a minimal package.json + entry point inside the target dir
    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "dir-flag-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        project_dir.join("index.ts"),
        r#"export default { fetch() { return new Response("ok"); } };"#,
    )
    .unwrap();

    // Invoke from root_dir, but tell tako which config file to create.
    let output = run_tako(&["-c", "my-app/tako.toml", "init"], &root_dir);

    assert!(
        output.status.success(),
        "tako -c DIR/tako.toml init failed: {}",
        stderr_str(&output)
    );

    assert!(
        project_dir.join("tako.toml").exists(),
        "tako.toml should be created in target dir"
    );
    assert!(
        !root_dir.join("tako.toml").exists(),
        "tako.toml should not be created in invocation directory"
    );
}

#[test]
fn test_init_accepts_config_flag_without_toml_suffix() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "suffixless-config-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        project_dir.join("index.ts"),
        r#"export default { fetch() { return new Response("ok"); } };"#,
    )
    .unwrap();

    let output = run_tako(&["-c", "preview", "init"], &project_dir);

    assert!(
        output.status.success(),
        "tako -c preview init failed: {}",
        stderr_str(&output)
    );

    assert!(
        project_dir.join("preview.toml").exists(),
        "preview.toml should be created when config suffix is omitted"
    );
    assert!(
        !project_dir.join("preview").exists(),
        "suffixless config argument should not create a file without .toml"
    );
}

#[test]
fn test_init_existing_config_in_non_interactive_mode_reports_cancellation() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "existing-config-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        project_dir.join("index.ts"),
        r#"export default { fetch() { return new Response("ok"); } };"#,
    )
    .unwrap();

    let existing = "name = \"existing\"\n";
    let config_path = project_dir.join("tako.toml");
    fs::write(&config_path, existing).unwrap();

    let output = run_tako(&["init"], &project_dir);

    assert!(
        output.status.success(),
        "tako init should exit successfully when overwrite is cancelled: {}",
        stderr_str(&output)
    );

    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("Operation cancelled"),
        "expected shared cancellation message: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(config_path).unwrap(),
        existing,
        "existing config should remain unchanged"
    );
}

#[test]
fn test_init_with_bun_detection() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    // Create bun.lockb to indicate Bun project
    fs::write(project_dir.join("bun.lockb"), "").unwrap();
    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "bun-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        project_dir.join("index.ts"),
        r#"export default { fetch() { return new Response("ok"); } };"#,
    )
    .unwrap();

    let output = run_tako(&["init"], &project_dir);

    assert!(
        output.status.success(),
        "tako init failed: {}",
        stderr_str(&output)
    );
    let content = fs::read_to_string(project_dir.join("tako.toml")).unwrap();
    assert!(
        content.contains("runtime = \"bun@"),
        "expected pinned bun runtime in tako.toml: {}",
        content
    );
}

#[test]
fn test_init_without_package_json() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    let output = run_tako(&["init"], &project_dir);

    // Should handle missing package.json gracefully
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    // Either fails or warns - both are acceptable
    assert!(!combined.is_empty(), "Should produce some output");
}
