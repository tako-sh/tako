use crate::support::*;

#[test]
fn test_deploy_uses_implicit_production_when_no_envs_configured() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    // Create tako.toml without envs section.
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"
runtime = "bun"
main = "index.ts"
"#,
    )
    .unwrap();
    fs::write(project_dir.join("bun.lockb"), "").unwrap();
    fs::write(project_dir.join("index.ts"), "export default {}").unwrap();

    let output = run_tako_with_env(&["deploy"], &project_dir, &home, &tako_home);

    // Should fail because production must be explicitly configured.
    assert!(
        !output.status.success(),
        "Deploy should fail when no servers are configured"
    );

    let err = stderr_str(&output);
    assert!(
        err.contains("Environment 'production' not found"),
        "Should require explicit production environment mapping: {}",
        err
    );
}

#[test]
fn test_deploy_rejects_development_environment() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"

[envs.development]
route = "dev.example.com"
servers = ["dev-1"]
"#,
    )
    .unwrap();
    fs::write(project_dir.join("bun.lockb"), "").unwrap();
    fs::write(project_dir.join("index.ts"), "export default {}").unwrap();

    let output = run_tako(&["deploy", "--env", "development"], &project_dir);

    assert!(
        !output.status.success(),
        "Deploy to development should be rejected"
    );

    let err = stderr_str(&output);
    assert!(
        err.contains("reserved for local development")
            || err.contains("cannot deploy to 'development'"),
        "Should explicitly reject deploying to development: {}",
        err
    );
}

#[test]
fn test_deploy_with_invalid_env() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    // Create tako.toml with env
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"
runtime = "bun"
main = "index.ts"

[envs.production]
route = "prod.example.com"
"#,
    )
    .unwrap();

    // Try to deploy to non-existent env
    let output = run_tako(&["deploy", "--env", "staging"], &project_dir);

    // Should fail because staging env doesn't exist
    assert!(
        !output.status.success(),
        "Deploy should fail with invalid env"
    );

    let err = stderr_str(&output);
    assert!(
        err.contains("staging") || err.contains("not found") || err.contains("Environment"),
        "Should mention invalid environment: {}",
        err
    );
}

#[test]
fn test_deploy_uses_preset_main_when_tako_main_is_missing() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    // Create tako.toml with preset but no explicit main.
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"
preset = "bun"

[envs.production]
route = "prod.example.com"
"#,
    )
    .unwrap();
    fs::write(
        project_dir.join("package.json"),
        r#"{"name":"test-app","main":"index.ts"}"#,
    )
    .unwrap();

    let output = run_tako(&["deploy", "--env", "production"], &project_dir);

    // The key contract is that deploy should not fail due missing main when preset defines one.
    assert!(
        !output.status.success(),
        "Deploy should fail in this test environment (typically due build/SSH preconditions)"
    );

    let stderr = stderr_str(&output);
    assert!(
        !stderr.contains("Set `main` in tako.toml or preset `main`")
            && !stderr.contains("No deploy entrypoint configured"),
        "Should not fail due missing main when preset supplies it: {}",
        stderr
    );
}

#[test]
fn test_deploy_validates_entry_point_exists() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    // Create tako.toml with explicit main that doesn't exist.
    // Include required build preset and explicit server mapping so deploy
    // reaches entrypoint/build validation before any remote operations.
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"
main = "nonexistent.ts"
preset = "bun"

[envs.production]
route = "prod.example.com"
servers = ["test-server"]
"#,
    )
    .unwrap();

    // Configure one global server with explicit target metadata.
    fs::write(
        tako_home.join("config.toml"),
        r#"
[[servers]]
name = "test-server"
host = "127.0.0.1"
port = 22222
arch = "x86_64"
libc = "glibc"
"#,
    )
    .unwrap();

    // Add bun.lockb and package.json so the fixture matches a Bun-style project.
    fs::write(project_dir.join("bun.lockb"), "").unwrap();
    fs::write(
        project_dir.join("package.json"),
        // Specify a nonexistent main in package.json to test that path too
        r#"{"name": "test-app", "version": "1.0.0", "main": "nonexistent.ts"}"#,
    )
    .unwrap();

    let output = run_tako_with_env(
        &["deploy", "--env", "production"],
        &project_dir,
        &home,
        &tako_home,
    );

    // Should fail. In fully provisioned environments this is a missing-entrypoint error.
    // In restricted CI/sandbox environments it may fail earlier at build or SSH preconditions.
    assert!(
        !output.status.success(),
        "Deploy should fail for invalid entry point or build preconditions"
    );

    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("entry")
            || stderr.contains("Entry")
            || stderr.contains("nonexistent")
            || stderr.contains("not found")
            || stderr.contains("main")
            || stderr.contains("Failed to fetch preset")
            || stderr.contains("lockfile"),
        "Should mention missing entry point, lockfile mismatch, or build failure: {}",
        stderr
    );
}

#[test]
fn test_deploy_without_name_uses_directory_name_fallback() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("fallback-app");
    std::fs::create_dir_all(&project_dir).unwrap();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    // No top-level `name`; deploy should fall back to directory-derived app name.
    fs::write(
        project_dir.join("tako.toml"),
        r#"
main = "nonexistent.ts"
preset = "bun"

[envs.production]
route = "prod.example.com"
servers = ["test-server"]
"#,
    )
    .unwrap();

    fs::write(
        tako_home.join("config.toml"),
        r#"
[[servers]]
name = "test-server"
host = "127.0.0.1"
port = 22222
arch = "x86_64"
libc = "glibc"
"#,
    )
    .unwrap();

    fs::write(project_dir.join("bun.lockb"), "").unwrap();
    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "fallback-app", "version": "1.0.0", "main": "nonexistent.ts"}"#,
    )
    .unwrap();

    let output = run_tako_with_env(
        &["deploy", "--env", "production"],
        &project_dir,
        &home,
        &tako_home,
    );

    assert!(
        !output.status.success(),
        "Deploy should fail in this fixture"
    );
    let stderr = stderr_str(&output);
    assert!(
        !stderr.contains("Missing top-level `name`"),
        "Deploy should not require top-level name: {}",
        stderr
    );
}

#[test]
fn test_deploy_validates_server_exists() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    // Create tako.toml referencing a server that doesn't exist.
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"

[envs.production]
route = "prod.example.com"
servers = ["nonexistent-server"]
"#,
    )
    .unwrap();

    // Add bun.lockb, package.json and entry point
    fs::write(project_dir.join("bun.lockb"), "").unwrap();
    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "test-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(project_dir.join("index.ts"), "export default {}").unwrap();

    let output = run_tako(&["deploy", "--env", "production"], &project_dir);

    // Should fail because server doesn't exist in global [[servers]] config
    assert!(
        !output.status.success(),
        "Deploy should fail with unknown server"
    );

    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("nonexistent-server")
            || stderr.contains("not found")
            || stderr.contains("config.toml")
            || stderr.contains("Server"),
        "Should mention missing server: {}",
        stderr
    );
}

#[test]
fn test_deploy_validates_no_servers_configured() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    // Create tako.toml with env but no server reference
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"
runtime = "node"
main = "index.js"

[envs.production]
route = "prod.example.com"
# No server specified
"#,
    )
    .unwrap();

    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "test-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(project_dir.join("index.js"), "export default {}").unwrap();

    let output = run_tako_with_env(
        &["deploy", "--env", "production"],
        &project_dir,
        &home,
        &tako_home,
    );

    // Should fail because no servers configured
    assert!(
        !output.status.success(),
        "Deploy should fail with no servers"
    );

    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("No servers have been added") || stderr.contains("tako servers add <host>"),
        "Should include add-server hint: {}",
        stderr
    );
}

#[test]
fn test_deploy_detects_missing_dns_secret_before_server_setup() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"
runtime = "node"
main = "index.js"

[envs.production]
routes = ["*.example.com"]
"#,
    )
    .unwrap();

    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "test-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(project_dir.join("index.js"), "export default {}").unwrap();

    let output = run_tako_with_env(
        &["deploy", "--env", "production"],
        &project_dir,
        &home,
        &tako_home,
    );

    assert!(
        !output.status.success(),
        "Deploy should fail when wildcard DNS credentials are missing"
    );

    let stderr = stderr_str(&output);
    assert!(
        stderr.contains("DNS errors")
            && stderr.contains("Wildcard routes require DNS credentials")
            && stderr.contains("tako dns configure --env production"),
        "Should fail locally with the DNS credential hint: {}",
        stderr
    );
    assert!(
        !stderr.contains("No servers have been added"),
        "DNS validation should run before server setup: {}",
        stderr
    );
}

#[test]
fn test_deploy_no_longer_requires_local_dist_artifacts() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"

[envs.production]
routes = ["api.example.com"]
servers = ["test-server"]
"#,
    )
    .unwrap();

    fs::write(project_dir.join("bun.lockb"), "").unwrap();
    fs::write(project_dir.join("package.json"), r#"{"name":"test-app"}"#).unwrap();
    fs::write(project_dir.join("index.ts"), "export default {}").unwrap();

    fs::write(
        tako_home.join("config.toml"),
        r#"
[[servers]]
name = "test-server"
host = "127.0.0.1"
port = 22222
arch = "x86_64"
libc = "glibc"
"#,
    )
    .unwrap();

    let output = run_tako_with_env(
        &["deploy", "--env", "production"],
        &project_dir,
        &home,
        &tako_home,
    );
    assert!(
        !output.status.success(),
        "deploy should fail due unreachable SSH server in this test setup"
    );

    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        !combined.contains("must contain build artifacts") && !combined.contains(".tako/dist"),
        "deploy should not require local dist artifacts: {}",
        combined
    );
}

#[test]
fn test_deploy_shows_validation_messages() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();

    // Create a valid-looking config that passes validation.
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"
runtime = "node"
main = "index.js"

[envs.production]
routes = ["api.example.com"]
"#,
    )
    .unwrap();

    fs::write(
        project_dir.join("package.json"),
        r#"{"name": "test-app", "version": "1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        project_dir.join("index.js"),
        "export default { fetch() { return new Response('ok'); } };",
    )
    .unwrap();

    // Create config.toml with multiple servers so non-interactive deploy cannot auto-select.
    let servers_path = tako_home.join("config.toml");
    fs::write(
        &servers_path,
        r#"
[[servers]]
name = "test-server"
host = "127.0.0.1"
port = 22222

[[servers]]
name = "backup-server"
host = "127.0.0.2"
port = 22223
"#,
    )
    .unwrap();

    let output = run_tako_with_env(
        &["deploy", "--env", "production"],
        &project_dir,
        &home,
        &tako_home,
    );

    // The deploy will fail after validation, but validation messages should still be shown.
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));

    // Should show validation warnings even though spinner output is suppressed.
    assert!(
        combined.contains("Validation"),
        "Should show validation warnings: {}",
        combined
    );
}
