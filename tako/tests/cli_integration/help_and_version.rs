use crate::support::*;

#[test]
fn test_help_shows_commands() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    let output = run_tako(&["--help"], &project_dir);

    assert!(output.status.success(), "help should succeed");

    let out = stdout_str(&output);
    assert!(out.contains("init"), "Should list init command");
    assert!(out.contains("deploy"), "Should list deploy command");
    assert!(out.contains("dev"), "Should list dev command");
    assert!(out.contains("run"), "Should list run command");
    assert!(out.contains("doctor"), "Should list doctor command");
    assert!(out.contains("upgrade"), "Should list upgrade command");
    assert!(out.contains("delete"), "Should list delete command");
    assert!(out.contains("servers"), "Should list servers command");
    assert!(out.contains("secrets"), "Should list secrets command");
}

#[test]
fn test_version_shows_version() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    let output = run_tako(&["--version"], &project_dir);

    assert!(output.status.success(), "version should succeed");

    let out = stdout_str(&output);
    assert!(
        out.contains("tako") || out.contains("0."),
        "Should show version: {}",
        out
    );
}
