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
fn test_deploy_requires_network_before_building() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("tako.toml"), "name='app'\nmain='missing.js'\nruntime='node'\n[envs.production]\nroute='app.example.com'\n").unwrap();
    let output = run_tako(&["deploy", "--env", "production"], temp.path());
    assert!(!output.status.success());
    assert!(
        stderr_str(&output).contains("network ID"),
        "{}",
        stderr_str(&output)
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
        stderr.contains("network ID in tako.toml"),
        "Should include add-server hint: {}",
        stderr
    );
}

#[test]
fn test_deploy_prints_secret_validation_errors_once() {
    let server = super::shared_state::ApiServer::new();
    let project = server.project("secret-validation");
    fs::write(project.join("tako.toml"), format!("name='demo'\nimage='nginx:alpine'\n[envs.production]\nnetwork='{}'\nroute='demo.example.test'\n[envs.staging]\nnetwork='{}'\nroute='staging.example.test'\n", server.network, server.network)).unwrap();
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "set", "API_KEY", "--env", "staging"],
        Some("staging-only"),
    );
    let output = run_tako_with_env(
        &["deploy", "--env", "production"],
        &project,
        &server.owner_home,
        &server.owner_home,
    );
    assert!(!output.status.success());
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert_eq!(combined.matches("Secret errors:").count(), 1, "{combined}");
}

#[test]
fn test_deploy_validates_ssl_after_pulling_shared_secrets() {
    let server = super::shared_state::ApiServer::new();
    let project = server.project("ssl-validation");
    fs::write(project.join("tako.toml"), format!("name='demo'\nimage='nginx:alpine'\n[envs.production]\nnetwork='{}'\nroutes=['*.example.com']\n", server.network)).unwrap();
    let output = server.run(
        &server.owner_home,
        &project,
        &["deploy", "--env", "production"],
        None,
    );
    assert!(!output.status.success());
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));
    assert!(
        combined.contains("SSL errors") && combined.contains("ssl.cloudflare"),
        "{combined}"
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
