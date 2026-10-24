use super::*;
use tempfile::TempDir;

#[test]
fn detects_project_and_home_clients_without_global_writes() {
    let project = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    std::fs::create_dir(project.path().join(".cursor")).unwrap();
    std::fs::create_dir(home.path().join(".codex")).unwrap();
    assert_eq!(
        detect(project.path(), Some(home.path())),
        vec![Agent::Codex, Agent::Cursor]
    );
}

#[test]
fn codex_settings_and_comments_survive_setup() {
    let project = TempDir::new().unwrap();
    std::fs::create_dir(project.path().join(".codex")).unwrap();
    std::fs::write(
        project.path().join(".codex/config.toml"),
        "# Keep this\nmodel = \"custom\"\n[mcp_servers.other]\ncommand = \"other\"\n[mcp_servers.tako.env]\nTAKO_HOME = \"custom-home\"\n",
    )
    .unwrap();
    let plan = files::plan(
        project.path(),
        &project.path().join("preview.toml"),
        Path::new("/bin/tako"),
        &[Agent::Codex],
    )
    .unwrap();
    for file in plan {
        file.write().unwrap();
    }
    let content = std::fs::read_to_string(project.path().join(".codex/config.toml")).unwrap();
    assert!(content.contains("# Keep this"));
    let content: toml::Value = toml::from_str(&content).unwrap();
    assert_eq!(content["model"].as_str(), Some("custom"));
    assert_eq!(
        content["mcp_servers"]["other"]["command"].as_str(),
        Some("other")
    );
}

#[test]
fn jsonc_settings_accept_comments_and_trailing_commas() {
    let project = TempDir::new().unwrap();
    std::fs::create_dir(project.path().join(".vscode")).unwrap();
    std::fs::write(
        project.path().join(".vscode/mcp.json"),
        "{ // a comment\n\"servers\": {\"other\": {\"command\":\"other\",},},}",
    )
    .unwrap();
    let plan = files::plan(
        project.path(),
        &project.path().join("tako.toml"),
        Path::new("/bin/tako"),
        &[Agent::GithubCopilot],
    )
    .unwrap();
    for file in plan {
        file.write().unwrap();
    }
    let text = std::fs::read_to_string(project.path().join(".vscode/mcp.json")).unwrap();
    assert!(text.contains("// a comment"));
    let content = jsonc_parser::parse_to_serde_value(&text, &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(content["servers"]["other"]["command"], "other");
}

#[cfg(unix)]
#[test]
fn setup_does_not_follow_skill_directory_symlinks() {
    let project = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::create_dir(project.path().join(".agents")).unwrap();
    std::os::unix::fs::symlink(outside.path(), project.path().join(".agents/skills")).unwrap();
    assert!(
        files::plan(
            project.path(),
            &project.path().join("tako.toml"),
            Path::new("/bin/tako"),
            &[Agent::Codex]
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn refreshing_mcp_preserves_custom_tako_environment_settings() {
    let project = TempDir::new().unwrap();
    std::fs::create_dir(project.path().join(".codex")).unwrap();
    std::fs::create_dir(project.path().join(".cursor")).unwrap();
    std::fs::write(
        project.path().join(".codex/config.toml"),
        "[mcp_servers.tako.env]\nTAKO_HOME = \"custom-home\"\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join(".cursor/mcp.json"),
        r#"{"mcpServers":{"tako":{"command":"old","env":{"TAKO_HOME":"custom-home"}}}}"#,
    )
    .unwrap();
    for file in files::plan(
        project.path(),
        &project.path().join("tako.toml"),
        Path::new("/bin/tako"),
        &[Agent::Codex, Agent::Cursor],
    )
    .unwrap()
    {
        file.write().unwrap();
    }
    let codex: toml::Value = toml::from_str(
        &std::fs::read_to_string(project.path().join(".codex/config.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        codex["mcp_servers"]["tako"]["env"]["TAKO_HOME"].as_str(),
        Some("custom-home")
    );
    let cursor: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.path().join(".cursor/mcp.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cursor["mcpServers"]["tako"]["env"]["TAKO_HOME"],
        "custom-home"
    );
}

#[cfg(unix)]
#[test]
fn refreshing_skills_preserves_skills_cli_links_into_project_canonical_directory() {
    let project = TempDir::new().unwrap();
    let canonical = project.path().join(".agents/skills/tako");
    std::fs::create_dir_all(&canonical).unwrap();
    std::fs::write(canonical.join("SKILL.md"), "old skill").unwrap();
    std::fs::create_dir_all(project.path().join(".claude/skills")).unwrap();
    let link = project.path().join(".claude/skills/tako");
    std::os::unix::fs::symlink("../../.agents/skills/tako", &link).unwrap();
    for file in files::plan(
        project.path(),
        &project.path().join("tako.toml"),
        Path::new("/bin/tako"),
        &[Agent::Codex, Agent::ClaudeCode],
    )
    .unwrap()
    {
        file.write().unwrap();
    }
    assert!(
        std::fs::symlink_metadata(link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::read_to_string(canonical.join("SKILL.md"))
            .unwrap()
            .contains("# Working with Tako")
    );
}
