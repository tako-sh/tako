use super::support::*;
use std::io::{BufRead, BufReader, Write};

#[test]
fn agents_installs_project_skills_and_preserves_client_settings_on_rerun() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join(".vscode")).unwrap();
    fs::write(
        project.path().join(".vscode/mcp.json"),
        r#"{"servers":{"other":{"command":"other"}},"inputs":[]}"#,
    )
    .unwrap();
    fs::write(
        project.path().join("AGENTS.md"),
        "# Project\n\nKeep this guidance.\n",
    )
    .unwrap();
    for _ in 0..2 {
        let result = run_tako(
            &[
                "agents",
                "--agent",
                "codex,claude-code,cursor,github-copilot",
                "--yes",
                "--json",
            ],
            project.path(),
        );
        assert!(result.status.success(), "{}", stderr_str(&result));
        let result: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(result["ok"], true);
        assert!(result["files"].as_array().unwrap().len() >= 8);
    }
    let instructions = fs::read_to_string(project.path().join("AGENTS.md")).unwrap();
    assert!(instructions.contains("Keep this guidance."));
    assert_eq!(
        instructions.matches("<!-- tako:agents:start -->").count(),
        1
    );
    assert!(project.path().join(".agents/skills/tako/SKILL.md").exists());
    assert!(
        project
            .path()
            .join(".claude/skills/tako-sdk-js/SKILL.md")
            .exists()
    );
    let vscode: serde_json::Value =
        serde_json::from_slice(&fs::read(project.path().join(".vscode/mcp.json")).unwrap())
            .unwrap();
    assert_eq!(vscode["servers"]["other"]["command"], "other");
    assert_eq!(vscode["servers"]["tako"]["type"], "stdio");
    let codex: toml::Value =
        toml::from_str(&fs::read_to_string(project.path().join(".codex/config.toml")).unwrap())
            .unwrap();
    assert_eq!(
        codex["mcp_servers"]["tako"]["args"][0].as_str(),
        Some("mcp")
    );
}

#[test]
fn agents_dry_run_lists_files_without_writing() {
    let project = TempDir::new().unwrap();
    let result = run_tako(
        &["agents", "--agent", "codex", "--dry-run", "--json"],
        project.path(),
    );
    assert!(result.status.success(), "{}", stderr_str(&result));
    let data: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(data["dry_run"], true);
    assert!(!data["files"].as_array().unwrap().is_empty());
    assert_eq!(fs::read_dir(project.path()).unwrap().count(), 0);
}

#[test]
fn agents_invalid_existing_config_fails_before_writing_any_files() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join(".cursor")).unwrap();
    fs::write(project.path().join(".cursor/mcp.json"), "invalid json").unwrap();
    let result = run_tako(
        &["agents", "--agent", "codex,cursor", "--yes", "--json"],
        project.path(),
    );
    assert!(!result.status.success());
    assert!(!project.path().join(".codex").exists());
    assert!(!project.path().join("AGENTS.md").exists());
}

#[test]
fn mcp_serves_tools_documentation_and_cli_results_over_stdio() {
    let project = TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tako"))
        .args(["mcp"])
        .current_dir(project.path())
        .env("TAKO_TELEMETRY", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    fn request(
        input: &mut impl Write,
        output: &mut impl BufRead,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "MCP closed stdout"
            );
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }
    let init = request(
        &mut input,
        &mut output,
        1,
        "initialize",
        serde_json::json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "tako");
    writeln!(
        input,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    let tools = request(
        &mut input,
        &mut output,
        2,
        "tools/list",
        serde_json::json!({}),
    );
    let tools = tools["result"]["tools"].as_array().unwrap();
    for name in [
        "tako_logs",
        "tako_doctor",
        "tako_deploy",
        "tako_run",
        "tako_secrets_set",
        "tako_backups_restore",
    ] {
        assert!(
            tools.iter().any(|tool| tool["name"] == name),
            "missing {name}"
        );
    }
    let docs = request(
        &mut input,
        &mut output,
        3,
        "resources/read",
        serde_json::json!({"uri":"tako://docs/troubleshooting"}),
    );
    assert!(
        docs["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("tako doctor")
    );
    let version = request(
        &mut input,
        &mut output,
        4,
        "tools/call",
        serde_json::json!({"name":"tako_version","arguments":{}}),
    );
    assert_eq!(version["result"]["isError"], false);
    assert!(
        version["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("version")
    );
    drop(input);
    assert!(child.wait().unwrap().success());
}

#[test]
fn mcp_startup_errors_keep_stdout_reserved_for_protocol_even_with_json() {
    let project = TempDir::new().unwrap();
    let result = run_tako(
        &["mcp", "--json", "-c", "missing/project.toml"],
        project.path(),
    );
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(stderr_str(&result).contains("Config directory does not exist"));
}

#[test]
fn agents_use_explicit_config_directory_and_require_confirmation_in_ci() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("app")).unwrap();
    let result = run_tako(
        &["agents", "-c", "app/preview", "--agent", "codex", "--ci"],
        project.path(),
    );
    assert!(!result.status.success());
    assert!(!project.path().join("app/.codex").exists());
    let result = run_tako(
        &["agents", "-c", "app/preview", "--agent", "codex", "--yes"],
        project.path(),
    );
    assert!(result.status.success(), "{}", stderr_str(&result));
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(project.path().join("app/.codex/config.toml")).unwrap())
            .unwrap();
    assert!(
        config["mcp_servers"]["tako"]["args"][2]
            .as_str()
            .unwrap()
            .ends_with("app/preview.toml")
    );
    assert!(!project.path().join(".codex").exists());
}
