use super::*;

#[test]
fn readiness_failure_hint_for_dev_command_detects_vite_commands() {
    for cmd in [
        vec!["vite".to_string()],
        vec!["vite".to_string(), "dev".to_string()],
        vec![
            "bun".to_string(),
            "--bun".to_string(),
            "./node_modules/.bin/vite".to_string(),
            "dev".to_string(),
        ],
    ] {
        let hint = readiness_failure_hint_for_dev_command(&cmd).unwrap();
        assert!(hint.contains("tako.sh/vite"));
    }
}

#[test]
fn readiness_failure_hint_for_dev_command_ignores_package_scripts() {
    let cmd = vec!["bun".to_string(), "run".to_string(), "dev".to_string()];

    assert!(readiness_failure_hint_for_dev_command(&cmd).is_none());
}

#[test]
fn dev_startup_lines_quiet_is_short() {
    let lines = dev_startup_lines(
        false,
        "app",
        "fake",
        Path::new("index.ts"),
        "https://app.test:8443/",
    );
    assert_eq!(lines[0], "https://app.test:8443/");
    assert!(lines.iter().all(|l| !l.contains("Tako Dev Server")));
}

#[test]
fn dev_startup_lines_verbose_includes_banner() {
    let lines = dev_startup_lines(
        true,
        "app",
        "fake",
        Path::new("index.ts"),
        "https://app.test:8443/",
    );
    assert!(lines.iter().any(|l| l == "Tako Dev Server"));
    assert!(lines.iter().any(|l| l.starts_with("URL:")));
}
