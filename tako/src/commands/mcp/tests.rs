use super::*;
use serde_json::json;

#[test]
fn schemas_and_argument_translation_follow_cli_definitions() {
    let catalog = catalog::catalog();
    let deploy = &catalog["tako_deploy"];
    assert_eq!(
        deploy.tool.input_schema["properties"]["yes"]["type"],
        "boolean"
    );
    assert_eq!(
        deploy
            .arguments(
                json!({"env":"preview", "yes":true, "dry_run":true})
                    .as_object()
                    .unwrap()
            )
            .unwrap(),
        ["deploy", "--env=preview", "--yes", "--dry-run"]
    );
    let scale = &catalog["tako_scale"];
    assert!(
        scale
            .arguments(json!({"instances":999}).as_object().unwrap())
            .is_err()
    );
    assert!(
        scale
            .arguments(json!({"instances":1,"unexpected":"x"}).as_object().unwrap())
            .is_err()
    );
    assert_eq!(
        scale.tool.input_schema["properties"]["instances"]["type"],
        "integer"
    );
    assert!(
        catalog["tako_logs"]
            .arguments(json!({"tail":true}).as_object().unwrap())
            .is_err()
    );
    assert_eq!(
        catalog["tako_logs"]
            .tool
            .annotations
            .as_ref()
            .unwrap()
            .read_only_hint,
        Some(true)
    );
    assert_eq!(
        deploy.tool.annotations.as_ref().unwrap().read_only_hint,
        Some(false)
    );
}

#[test]
fn run_keeps_script_arguments_as_literal_values() {
    let catalog = catalog::catalog();
    let args = catalog["tako_run"]
        .arguments(
            json!({"command":["echo", "$(touch /tmp/never)", "--flag"]})
                .as_object()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(args, ["run", "--", "echo", "$(touch /tmp/never)", "--flag"]);
}

#[test]
fn bundled_docs_and_skills_are_discoverable_and_searchable() {
    assert!(
        docs::content("docs/agents")
            .unwrap()
            .contains("tako agents")
    );
    assert!(docs::content("skills/tako-sdk-rust").is_some());
    for topic in [
        "secrets",
        "storage",
        "images",
        "cache",
        "servers",
        "routing",
        "operations",
        "backups",
    ] {
        assert!(docs::content(&format!("docs/{topic}")).is_some(), "{topic}");
    }
    let topics =
        serde_json::to_string(&docs::call("tako_docs", &Default::default()).unwrap()).unwrap();
    assert!(topics.contains("docs/secrets: # Secrets"));
    assert!(topics.contains("docs/index: # Documentation"));
    assert!(topics.contains("docs/tako-toml: # tako.toml Reference"));
    assert!(topics.contains("skills/tako: # Working with Tako"));
    for topic in [
        "nextjs",
        "tanstack-start",
        "go-http",
        "axum",
        "docker",
        "dockerfile",
        "docker-workflows",
    ] {
        assert!(docs::content(&format!("docs/{topic}")).is_some(), "{topic}");
    }
    for topic in ["secrets", "presence", "stores"] {
        let source = docs::content(&format!("docs/{topic}")).unwrap();
        for variant in ["```ts JavaScript", "```go Go", "```rust Rust"] {
            assert!(source.contains(variant), "{topic}: {variant}");
        }
    }
    assert!(
        docs::call(
            "tako_search_docs",
            json!({"query":"TLS"}).as_object().unwrap()
        )
        .unwrap()
        .len()
            == 1
    );
    assert!(
        docs::call(
            "tako_docs",
            json!({"topic":"../../etc/passwd"}).as_object().unwrap()
        )
        .is_err()
    );
}

#[tokio::test]
async fn subprocess_timeout_terminates_local_execution() {
    let dir = tempfile::TempDir::new().unwrap();
    let script = dir.path().join("slow");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = std::time::Instant::now();
    let result = process::execute(
        &script,
        dir.path(),
        &dir.path().join("tako.toml"),
        &[],
        None,
        Duration::from_millis(100),
    )
    .await;
    assert!(result.unwrap_err().contains("timed out"));
    assert!(before.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn local_log_snapshot_reads_backlog_with_cursor_and_closes_subscription() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (client, server) = tokio::net::UnixStream::pair().unwrap();
    let server = tokio::spawn(async move {
        let (reader, mut writer) = server.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["config_path"], "/project/preview.toml");
        assert_eq!(request["after"], 12);
        writer.write_all(b"{\"type\":\"LogsSubscribed\"}\n{\"type\":\"LogEntry\",\"id\":13,\"line\":\"request failed\"}\n{\"type\":\"LogEntry\",\"id\":14,\"line\":\"retry succeeded\"}\n").await.unwrap();
        line.clear();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    });
    let result = local_logs::snapshot(client, "/project/preview.toml", Some(12), 100)
        .await
        .unwrap();
    assert_eq!(result["logs"].as_array().unwrap().len(), 2);
    assert_eq!(result["next_after"], 14);
    assert_eq!(result["logs"][0]["record"], "request failed");
    server.await.unwrap();
}

#[tokio::test]
async fn subprocess_output_is_bounded_and_reports_truncation() {
    let dir = tempfile::TempDir::new().unwrap();
    let script = dir.path().join("verbose");
    std::fs::write(
        &script,
        "#!/bin/sh\nhead -c 1100000 /dev/zero | tr '\\000' x\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = process::execute(
        &script,
        dir.path(),
        &dir.path().join("tako.toml"),
        &[],
        None,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["truncated"], true);
    assert_eq!(
        result["stdout"].as_str().unwrap().len(),
        process::OUTPUT_LIMIT
    );
}

#[tokio::test]
async fn subprocess_receives_secret_stdin_without_command_interpolation() {
    let dir = tempfile::TempDir::new().unwrap();
    let script = dir.path().join("input");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > received\nprintf '{\"ok\":true}'\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let secret = "a multiline\nsecret $(echo unsafe)\n";
    let result = process::execute(
        &script,
        dir.path(),
        &dir.path().join("tako.toml"),
        &[],
        Some(secret),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("received")).unwrap(),
        secret
    );
    assert_eq!(result["ok"], true);
    assert!(!result.to_string().contains(secret));
}

#[test]
fn dev_start_is_finite_and_dry_run_is_exposed_only_where_supported() {
    let catalog = catalog::catalog();
    assert_eq!(
        catalog["tako_dev"].arguments(&Default::default()).unwrap(),
        ["dev", "--background"]
    );
    assert!(
        catalog["tako_deploy"].tool.input_schema["properties"]
            .get("dry_run")
            .is_some()
    );
    assert!(
        catalog["tako_secrets_set"].tool.input_schema["properties"]
            .get("dry_run")
            .is_none()
    );
}

#[tokio::test]
async fn cancelling_subprocess_prevents_later_child_side_effects() {
    let dir = tempfile::TempDir::new().unwrap();
    let script = dir.path().join("cancel");
    std::fs::write(
        &script,
        "#!/bin/sh\ntouch started\n(sleep 1; touch unexpected) &\nwait\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let project = dir.path().to_path_buf();
    let task = tokio::spawn(async move {
        process::execute(
            &script,
            &project,
            &project.join("tako.toml"),
            &[],
            None,
            Duration::from_secs(5),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dir.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!dir.path().join("unexpected").exists());
}
