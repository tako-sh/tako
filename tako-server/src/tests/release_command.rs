use super::*;

// CodeQL[rust/cleartext-logging]: hardcoded fixture secrets in tests; set_secrets encrypts at rest and update_secrets logs only app name.
#[tokio::test]
async fn run_release_executes_command_in_release_dir() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    // App name uses '/' separator; filesystem encodes this as two directory components.
    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("production")
        .join("releases")
        .join("abc1234");
    std::fs::create_dir_all(&release_dir).unwrap();
    let manifest = serde_json::json!({
        "runtime": "bun",
        "main": "index.ts",
        "idle_timeout": 300,
        "app_dir": "",
        "env_vars": {"NODE_ENV": "production"},
    });
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Seed stale secrets for the app. The release command must use the
    // command payload, because deploy sends fresh secrets before Deploy stores
    // them in the state DB.
    state
        .state_store
        .set_secrets(
            "my-app/production",
            &HashMap::from([("DATABASE_URL".to_string(), "postgres://old".to_string())]),
        )
        .unwrap();

    let response = state
        .handle_command(Command::RunRelease {
            app: "my-app/production".to_string(),
            version: "abc1234".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            command_line: "printf %s \"$NODE_ENV-$DATABASE_URL\" > out.txt".to_string(),
            vars: HashMap::new(),
            secrets: HashMap::from([("DATABASE_URL".to_string(), "postgres://new".to_string())]),
        })
        .await;

    let data = match response {
        Response::Ok { data } => data,
        Response::Error { message } => panic!("expected ok, got: {message}"),
    };
    assert_eq!(data.get("exit_code").and_then(|v| v.as_i64()), Some(0));

    let written = std::fs::read_to_string(release_dir.join("out.txt")).unwrap();
    assert_eq!(written, "production-postgres://new");
}

// CodeQL[rust/cleartext-logging]: hardcoded fixture secrets in tests; set_secrets encrypts at rest and update_secrets logs only app name.
#[tokio::test]
async fn run_release_returns_error_on_nonzero_exit() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("production")
        .join("releases")
        .join("abc1234");
    std::fs::create_dir_all(&release_dir).unwrap();
    let manifest = serde_json::json!({
        "runtime": "bun",
        "main": "index.ts",
        "idle_timeout": 300,
        "app_dir": "",
    });
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let response = state
        .handle_command(Command::RunRelease {
            app: "my-app/production".to_string(),
            version: "abc1234".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            command_line: "echo boom 1>&2; exit 3".to_string(),
            vars: HashMap::new(),
            secrets: HashMap::new(),
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected error response, got: {response:?}");
    };
    assert!(
        message.contains('3'),
        "expected exit code in message: {message}"
    );
    assert!(
        message.contains("boom"),
        "expected stderr in message: {message}"
    );
}

#[tokio::test]
async fn run_release_rejects_path_outside_release_root() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let response = state
        .handle_command(Command::RunRelease {
            app: "my-app/production".to_string(),
            version: "abc1234".to_string(),
            path: "/etc/passwd".to_string(),
            command_line: "true".to_string(),
            vars: HashMap::new(),
            secrets: HashMap::new(),
        })
        .await;

    assert!(matches!(response, Response::Error { .. }));
}
