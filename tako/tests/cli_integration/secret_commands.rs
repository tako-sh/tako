use crate::support::*;

pub(super) fn decrypt_test_secret(encrypted: &str, key_b64: &str) -> String {
    use aes_gcm::{
        Aes256Gcm, Nonce,
        aead::{Aead, KeyInit},
    };
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

    let key = BASE64.decode(key_b64.trim()).expect("decode secret key");
    let combined = BASE64.decode(encrypted).expect("decode encrypted secret");
    let (nonce, ciphertext) = combined.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("create cipher");
    let nonce = Nonce::try_from(nonce).expect("nonce contains 12 bytes");
    let plaintext = cipher.decrypt(&nonce, ciphertext).expect("decrypt secret");

    String::from_utf8(plaintext).expect("secret is UTF-8")
}

fn write_secret_test_tako_toml(path: &Path) {
    fs::write(
        path.join("tako.toml"),
        r#"
name = "test-app"
runtime = "bun"
main = "index.ts"

[envs.production]
route = "prod.example.com"
"#,
    )
    .unwrap();
}

#[test]
fn test_secret_ls_empty() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    // Create tako.toml
    fs::write(
        project_dir.join("tako.toml"),
        r#"
name = "test-app"
runtime = "bun"
main = "index.ts"
"#,
    )
    .unwrap();

    let output = run_tako(&["secrets", "list"], &project_dir);

    assert!(
        output.status.success(),
        "tako secrets list failed: {}",
        stderr_str(&output)
    );

    let out = stdout_str(&output);
    assert!(
        out.contains("No secrets") || out.is_empty() || out.contains("0 secrets"),
        "Should show no secrets"
    );
}

#[test]
fn test_secret_ls_prints_table_without_section_heading() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();
    write_secret_test_tako_toml(&project_dir);

    let set_output = run_tako_with_stdin_and_env(
        &["secrets", "set", "API_KEY", "--env", "production"],
        &project_dir,
        "secret123\n",
        &home,
        &tako_home,
    );
    assert!(
        set_output.status.success(),
        "secret set should succeed: {}{}",
        stdout_str(&set_output),
        stderr_str(&set_output)
    );

    let output = run_tako_with_env(&["secrets", "list"], &project_dir, &home, &tako_home);
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));

    assert!(
        output.status.success(),
        "tako secrets list failed: {}",
        combined
    );
    assert!(
        combined.contains("API_KEY"),
        "expected secret name in list output: {combined}"
    );
    assert!(
        combined.contains("[set]"),
        "expected set marker in list output: {combined}"
    );
    assert!(
        !combined.lines().any(|line| line.trim() == "Secrets"),
        "list output should not include a Secrets section heading: {combined}"
    );
}

#[test]
fn test_secret_set_reads_from_stdin() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    // Create tako.toml with env section
    write_secret_test_tako_toml(&project_dir);

    // Set a secret - value comes from stdin
    let output = run_tako_with_stdin(
        &[
            "secrets",
            "set",
            "API_KEY",
            "--env",
            "production",
            "--expires-on",
            "2099-01-01",
        ],
        &project_dir,
        "secret123\n",
    );

    assert!(
        output.status.success(),
        "secret set should succeed: {}{}",
        stdout_str(&output),
        stderr_str(&output)
    );

    let secrets_path = project_dir.join(".tako").join("secrets.local.json");
    assert!(secrets_path.exists(), "secrets file should be created");

    let raw = fs::read_to_string(&secrets_path).expect("read secrets file");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("parse secrets json");
    let stored = parsed["values"]["production"]["app"]["API_KEY"]["value"]
        .as_str()
        .expect("stored API_KEY value");
    assert!(!stored.is_empty(), "stored value should not be empty");
    assert_ne!(stored, "secret123", "stored value should be encrypted");
    assert_eq!(
        parsed["values"]["production"]["app"]["API_KEY"]["expires_on"].as_str(),
        Some("2099-01-01")
    );
    // Key id should be present.
    assert!(
        parsed["values"]["production"]["key_id"].as_str().is_some(),
        "key id should be present"
    );
}

#[test]
fn test_secret_set_reads_multiline_value_from_stdin() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();
    write_secret_test_tako_toml(&project_dir);

    let secret_value = "-----BEGIN TOKEN-----\nline-one\nline-two\n-----END TOKEN-----\n";
    let output = run_tako_with_stdin_and_env(
        &["secrets", "set", "PRIVATE_TOKEN", "--env", "production"],
        &project_dir,
        secret_value,
        &home,
        &tako_home,
    );

    assert!(
        output.status.success(),
        "secret set should succeed: {}{}",
        stdout_str(&output),
        stderr_str(&output)
    );

    let raw = fs::read_to_string(project_dir.join(".tako").join("secrets.local.json"))
        .expect("read secrets file");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("parse secrets json");
    let key_id = parsed["values"]["production"]["key_id"]
        .as_str()
        .expect("production key id");
    let encrypted = parsed["values"]["production"]["app"]["PRIVATE_TOKEN"]["value"]
        .as_str()
        .expect("stored PRIVATE_TOKEN value");
    let key_b64 = fs::read_to_string(tako_home.join("keys").join(key_id)).expect("read key");

    assert_eq!(
        decrypt_test_secret(encrypted, &key_b64),
        "-----BEGIN TOKEN-----\nline-one\nline-two\n-----END TOKEN-----"
    );
}

#[test]
fn test_secret_set_omits_expiry_when_not_provided() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    write_secret_test_tako_toml(&project_dir);

    let output = run_tako_with_stdin(
        &["secrets", "set", "API_KEY", "--env", "production"],
        &project_dir,
        "secret123\n",
    );

    assert!(
        output.status.success(),
        "secret set should succeed without expiry: {}{}",
        stdout_str(&output),
        stderr_str(&output)
    );

    let raw = fs::read_to_string(project_dir.join(".tako").join("secrets.local.json"))
        .expect("read secrets file");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("parse secrets json");
    assert!(
        parsed["values"]["production"]["app"]["API_KEY"]
            .get("expires_on")
            .is_none(),
        "{parsed:#}"
    );
}

#[test]
fn test_secret_set_requires_env_when_non_interactive() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    write_secret_test_tako_toml(&project_dir);

    let output = run_tako_with_stdin(&["secrets", "set", "API_KEY"], &project_dir, "secret123\n");
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));

    assert!(
        !output.status.success(),
        "secret set without env should fail non-interactively: {}",
        combined
    );
    assert!(
        combined.contains("Missing required environment"),
        "expected missing environment error: {}",
        combined
    );
    assert!(
        !project_dir.join(".tako").join("secrets.json").exists(),
        "secrets file should not be created before environment selection completes"
    );
}

#[test]
fn test_secret_key_import_reads_bundle_from_stdin() {
    use base64::{
        Engine,
        engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64_URL},
    };

    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();
    write_secret_test_tako_toml(&project_dir);

    let key_id = "0123456789abcdef";
    fs::create_dir_all(project_dir.join(".tako")).unwrap();
    fs::write(
        project_dir.join(".tako").join("secrets.json"),
        format!(
            r#"{{
  "production": {{
"key_id": "{key_id}",
"app": {{}}
  }}
}}"#
        ),
    )
    .unwrap();

    let raw_key = [7u8; 32];
    let key_b64 = BASE64.encode(raw_key);
    let payload = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "id": key_id,
        "key": key_b64,
    }))
    .unwrap();
    let bundle = format!("{}\n", BASE64_URL.encode(payload));

    let output = run_tako_with_stdin_and_env(
        &["secrets", "key", "import"],
        &project_dir,
        &bundle,
        &home,
        &tako_home,
    );
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));

    assert!(
        output.status.success(),
        "key import should succeed: {}",
        combined
    );
    assert!(
        combined.contains("Imported production key."),
        "expected matching env in import output: {}",
        combined
    );
    assert_eq!(
        fs::read_to_string(tako_home.join("keys").join(key_id)).expect("read imported key"),
        key_b64
    );
}

#[test]
fn test_secret_key_import_accepts_env_for_exported_key() {
    use base64::{
        Engine,
        engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64_URL},
    };

    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();
    write_secret_test_tako_toml(&project_dir);

    let key_id = "0123456789abcdef";
    fs::create_dir_all(project_dir.join(".tako")).unwrap();
    fs::write(
        project_dir.join(".tako").join("secrets.json"),
        format!(
            r#"{{
  "production": {{
"key_id": "{key_id}",
"app": {{}}
  }}
}}"#
        ),
    )
    .unwrap();

    let raw_key = [7u8; 32];
    let key_b64 = BASE64.encode(raw_key);
    let payload = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "id": key_id,
        "key": key_b64,
    }))
    .unwrap();
    let bundle = format!("{}\n", BASE64_URL.encode(payload));

    let output = run_tako_with_stdin_and_env(
        &["secrets", "key", "import", "--env", "production"],
        &project_dir,
        &bundle,
        &home,
        &tako_home,
    );
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));

    assert!(
        output.status.success(),
        "key import should succeed: {}",
        combined
    );
    assert!(
        combined.contains("Imported production key."),
        "expected env in import output: {}",
        combined
    );
    assert_eq!(
        fs::read_to_string(tako_home.join("keys").join(key_id)).expect("read imported key"),
        key_b64
    );
}

#[test]
fn test_secret_key_import_passphrase_initializes_environment_key() {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tako_home).unwrap();
    write_secret_test_tako_toml(&project_dir);

    let output = run_tako_with_stdin_and_env(
        &[
            "secrets",
            "key",
            "import",
            "--passphrase",
            "--env",
            "production",
        ],
        &project_dir,
        "correct horse battery staple\n",
        &home,
        &tako_home,
    );
    let combined = format!("{}{}", stdout_str(&output), stderr_str(&output));

    assert!(
        output.status.success(),
        "passphrase import should succeed: {}",
        combined
    );
    assert!(
        combined.contains("Imported production key."),
        "expected passphrase import success output: {}",
        combined
    );

    let secrets_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(project_dir.join(".tako/secrets.local.json")).unwrap(),
    )
    .unwrap();
    let key_id = secrets_json["values"]["production"]["key_id"]
        .as_str()
        .expect("production key id");
    let expected_key = pbkdf2::pbkdf2_hmac_array::<sha2::Sha256, 32>(
        b"correct horse battery staple",
        format!("tako-secrets-v1:{key_id}").as_bytes(),
        600_000,
    );

    assert_eq!(
        fs::read_to_string(tako_home.join("keys").join(key_id)).expect("read imported key"),
        BASE64.encode(expected_key)
    );
}

#[test]
fn test_secret_sync_requires_network_even_without_local_secrets() {
    let temp = TempDir::new().unwrap();
    write_secret_test_tako_toml(temp.path());
    let output = run_tako(&["secrets", "sync", "--env", "production"], temp.path());
    assert!(!output.status.success());
    assert!(stderr_str(&output).contains("network ID in tako.toml"));
}

#[test]
fn test_secret_sync_reports_network_failure_and_preserves_draft() {
    let fixture = super::native_management::Fixture::with_servers(&["prod"]);
    let project = fixture.directory.path();
    fs::write(
        project.join("tako.toml"),
        format!(
            "name='demo'\n[envs.production]\nnetwork='{}'\nroute='demo.example.test'\n",
            fixture.profile.snapshot.network.id
        ),
    )
    .unwrap();
    let output = run_tako_with_stdin_and_env(
        &["secrets", "set", "API_KEY", "--env", "production"],
        project,
        "local-draft",
        &fixture.home,
        &fixture.tako_home,
    );
    assert!(output.status.success(), "{}", stderr_str(&output));
    let path = project.join(".tako/secrets.local.json");
    let before = fs::read(&path).unwrap();
    let draft: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(
        draft["environments"]["production"]["scope"],
        "demo/production"
    );
    assert_eq!(
        draft["environments"]["production"]["network"],
        fixture.profile.snapshot.network.id.to_string()
    );
    let output = run_tako_with_env(
        &["secrets", "sync", "--env", "production"],
        project,
        &fixture.home,
        &fixture.tako_home,
    );
    assert!(!output.status.success(), "{}", stdout_str(&output));
    assert_eq!(fs::read(path).unwrap(), before);
}
