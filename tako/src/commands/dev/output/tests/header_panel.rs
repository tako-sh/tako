use super::*;

#[test]
fn format_header_has_logo_and_version() {
    let h = format_header();
    assert!(h.contains('█'));
    let first_line = h.lines().next().unwrap();
    assert!(first_line.contains('v'));
}

#[test]
fn format_header_has_all_logo_rows() {
    let h = format_header();
    assert_eq!(h.lines().count(), LOGO_ROWS.len());
    for (line, row) in h.lines().zip(LOGO_ROWS.iter()) {
        assert!(line.contains(row));
    }
}

#[test]
fn format_panel_has_border_and_app_name_with_runtime() {
    let panel = format_panel(
        "myapp",
        "running",
        "bun",
        "user/myapp",
        "main",
        "apps/myapp",
        None,
        &["myapp.test".to_string()],
        443,
        None,
        None,
    );
    assert!(panel.contains('┌'));
    assert!(panel.contains('└'));
    assert!(panel.contains("myapp"));
}

#[test]
fn format_panel_shows_routes_label() {
    let panel = format_panel(
        "app",
        "running",
        "bun",
        "user/app",
        "main",
        "apps/app",
        None,
        &["app.test".to_string()],
        443,
        None,
        None,
    );
    let plain = strip_ansi(&panel);
    assert!(plain.contains("routes"));
    assert!(plain.contains("https://app.test"));
}

#[test]
fn format_panel_shows_all_urls() {
    let hosts = vec!["a.test".to_string(), "b.test".to_string()];
    let panel = format_panel(
        "app", "running", "bun", "u/r", "main", "", None, &hosts, 443, None, None,
    );
    let plain = strip_ansi(&panel);
    assert!(plain.contains("https://a.test"));
    assert!(plain.contains("https://b.test"));
}

#[test]
fn format_panel_shows_wildcard_and_path_routes() {
    let hosts = vec![
        "bun-example.test".to_string(),
        "bun-example.test/bun".to_string(),
        "*.bun-example.test".to_string(),
    ];
    let panel = format_panel_wide(
        "bun-example",
        "running",
        "bun",
        "u/r",
        "main",
        "",
        None,
        &hosts,
        443,
        None,
        None,
        120,
    );
    let plain = strip_ansi(&panel);
    assert!(
        plain.contains("https://bun-example.test/bun"),
        "missing /bun route"
    );
    assert!(
        plain.contains("https://*.bun-example.test"),
        "missing wildcard route"
    );
    assert_eq!(
        plain.matches("https://").count(),
        3,
        "expected exactly 3 route URLs"
    );
}

#[test]
fn format_panel_omits_443_port() {
    let panel = format_panel(
        "app",
        "running",
        "",
        "",
        "main",
        "",
        None,
        &["app.test".to_string()],
        443,
        None,
        None,
    );
    assert!(!strip_ansi(&panel).contains(":443"));
}

#[test]
fn format_panel_includes_custom_port() {
    let panel = format_panel_wide(
        "app",
        "running",
        "",
        "",
        "main",
        "",
        None,
        &["app.test".to_string()],
        47831,
        None,
        None,
        120,
    );
    assert!(strip_ansi(&panel).contains(":47831"));
}

#[test]
fn format_panel_shows_metrics() {
    let panel = format_panel(
        "app",
        "running",
        "",
        "",
        "main",
        "",
        None,
        &["app.test".to_string()],
        443,
        Some(50.0),
        Some(100 * 1024 * 1024),
    );
    let plain = strip_ansi(&panel);
    assert!(plain.contains("50%") || plain.contains("50"));
    assert!(plain.contains("100 MB"));
}

#[test]
fn format_panel_shows_dash_without_metrics() {
    let panel = format_panel(
        "app",
        "running",
        "",
        "",
        "main",
        "",
        None,
        &["app.test".to_string()],
        443,
        None,
        None,
    );
    assert!(strip_ansi(&panel).contains('—'));
}

#[test]
fn format_panel_shows_repo_info() {
    let panel = format_panel(
        "app",
        "running",
        "bun",
        "myorg/myrepo",
        "main",
        "apps/myapp",
        None,
        &["app.test".to_string()],
        443,
        None,
        None,
    );
    let plain = strip_ansi(&panel);
    assert!(plain.contains("myorg/myrepo/apps/myapp"));
    assert!(plain.contains("main"));
}

#[test]
fn format_panel_stacked_has_border_and_content() {
    let panel = format_panel_stacked(
        "app",
        "running",
        "bun",
        "user/repo",
        "main",
        "projects/app",
        None,
        &["app.test".to_string()],
        443,
        Some(25.0),
        Some(50 * 1024 * 1024),
        60,
    );
    let plain = strip_ansi(&panel);
    assert!(plain.contains('┌'));
    assert!(plain.contains('└'));
    assert!(plain.contains("app"));
    assert!(plain.contains("routes"));
    assert!(plain.contains("https://app.test"));
    assert!(plain.contains("cpu"));
    assert!(plain.contains("ram"));
}

#[test]
fn format_panel_shows_worktree_indicator() {
    let panel = format_panel(
        "app",
        "running",
        "bun",
        "user/repo",
        "main",
        "apps/app",
        Some("wt1"),
        &["app.test".to_string()],
        443,
        None,
        None,
    );
    let plain = strip_ansi(&panel);
    assert!(plain.contains("worktree (wt1)"));
}

#[test]
fn format_panel_omits_worktree_when_none() {
    let panel = format_panel(
        "app",
        "running",
        "bun",
        "user/repo",
        "main",
        "apps/app",
        None,
        &["app.test".to_string()],
        443,
        None,
        None,
    );
    let plain = strip_ansi(&panel);
    assert!(!plain.contains("worktree"));
}
