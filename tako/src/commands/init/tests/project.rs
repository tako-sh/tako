use tempfile::TempDir;

use super::super::project::{
    display_config_path_for_prompt, ensure_project_gitignore_tracks_secrets,
};

#[test]
fn display_config_path_for_prompt_uses_path_relative_to_current_directory() {
    let temp = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(temp.path()).unwrap();
    let config_path = cwd.join("tako.toml");
    std::fs::write(&config_path, "name = \"demo\"\n").unwrap();

    assert_eq!(
        display_config_path_for_prompt(&config_path, &cwd),
        "tako.toml"
    );
}

#[test]
fn display_config_path_for_prompt_keeps_subdirectory_when_relative_to_current_directory() {
    let temp = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(temp.path()).unwrap();
    let project_dir = cwd.join("apps/web");
    std::fs::create_dir_all(&project_dir).unwrap();
    let config_path = project_dir.join("preview.toml");
    std::fs::write(&config_path, "name = \"demo\"\n").unwrap();

    assert_eq!(
        display_config_path_for_prompt(&config_path, &cwd),
        "apps/web/preview.toml"
    );
}

#[test]
fn display_config_path_for_prompt_falls_back_to_absolute_path_outside_current_directory() {
    let temp = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(temp.path()).unwrap();

    let outside = TempDir::new().unwrap();
    let config_path = std::fs::canonicalize(outside.path())
        .unwrap()
        .join("preview.toml");
    std::fs::write(&config_path, "name = \"demo\"\n").unwrap();

    assert_eq!(
        display_config_path_for_prompt(&config_path, &cwd),
        config_path.display().to_string()
    );
}

#[test]
fn init_gitignore_uses_repo_root_for_nested_project() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path();
    let project_dir = repo_root.join("apps/web");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(repo_root.join(".git"), "gitdir: /tmp/fake-git-dir\n").unwrap();

    ensure_project_gitignore_tracks_secrets(&project_dir).unwrap();

    let gitignore = std::fs::read_to_string(repo_root.join(".gitignore")).unwrap();
    assert!(
        gitignore.contains("**/.tako/*\n!**/.tako/secrets.json\n"),
        "expected repo root .gitignore to contain global tako rules: {gitignore}"
    );
    assert!(
        !project_dir.join(".gitignore").exists(),
        "expected nested app .gitignore to remain untouched"
    );
}

#[test]
fn init_gitignore_falls_back_to_project_dir_outside_git_repo() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("app");
    std::fs::create_dir_all(&project_dir).unwrap();

    ensure_project_gitignore_tracks_secrets(&project_dir).unwrap();

    let gitignore = std::fs::read_to_string(project_dir.join(".gitignore")).unwrap();
    assert!(
        gitignore.contains("**/.tako/*\n!**/.tako/secrets.json\n"),
        "expected project-local .gitignore when no repo root is found: {gitignore}"
    );
}

#[test]
fn init_gitignore_does_not_duplicate_existing_rules() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    ensure_project_gitignore_tracks_secrets(&project_dir).unwrap();
    ensure_project_gitignore_tracks_secrets(&project_dir).unwrap();

    let gitignore = std::fs::read_to_string(project_dir.join(".gitignore")).unwrap();
    assert_eq!(
        gitignore.matches("!**/.tako/secrets.json").count(),
        1,
        "expected secrets tracking rule to remain deduplicated"
    );
}
