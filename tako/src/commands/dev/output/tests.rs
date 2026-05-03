use super::super::LogLevel;
use super::*;
use crate::commands::dev::output_render::{
    extract_repo_slug, fit_scope, format_lan_block, format_panel_stacked, format_panel_wide,
    progress_bar, vlen,
};
use crate::output::LOGO_ROWS;
use console::{measure_text_width, strip_ansi_codes, truncate_str};

#[test]
fn collect_process_tree_pids_includes_descendants() {
    let root = Pid::from_u32(10);
    let child = Pid::from_u32(11);
    let grandchild = Pid::from_u32(12);
    let unrelated = Pid::from_u32(99);
    let got = collect_process_tree_pids(
        &[
            (root, None),
            (child, Some(root)),
            (grandchild, Some(child)),
            (unrelated, None),
        ],
        root,
    );
    assert!(got.contains(&root));
    assert!(got.contains(&child));
    assert!(got.contains(&grandchild));
    assert!(!got.contains(&unrelated));
}

#[test]
fn collect_process_tree_pids_handles_parent_cycle() {
    let root = Pid::from_u32(1);
    let child = Pid::from_u32(2);
    let got = collect_process_tree_pids(&[(root, Some(child)), (child, Some(root))], root);
    assert_eq!(got.len(), 2);
}

#[test]
fn format_log_fields() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "app".to_string(),
        message: "hello".to_string(),
        fields: None,
        kind: None,
    };
    let out = format_log(&log);
    assert!(out.contains("12:34:56"));
    assert!(out.contains("INFO"));
    assert!(out.contains("app"));
    assert!(out.contains("hello"));
}

#[test]
fn format_log_aligns_continuation_lines_under_message_column() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "app".to_string(),
        message: "line1\nline2\nline3".to_string(),
        fields: None,
        kind: None,
    };
    let plain = strip_ansi(&format_log(&log));
    let lines: Vec<&str> = plain.split('\n').collect();
    assert_eq!(lines.len(), 3);

    let first = lines[0];
    let msg_col = first.find("line1").expect("first line contains msg");

    assert!(lines[1].starts_with(&" ".repeat(msg_col)));
    assert_eq!(&lines[1][msg_col..], "line2");
    assert!(lines[2].starts_with(&" ".repeat(msg_col)));
    assert_eq!(&lines[2][msg_col..], "line3");
}

#[test]
fn raw_terminal_block_resets_carriage_for_each_multiline_row() {
    let block = raw_terminal_block("alpha\nbeta\n  gamma");
    assert_eq!(block, "\ralpha\r\n\rbeta\r\n\r  gamma\r\n");
}

#[test]
fn format_log_appends_fields_as_key_value_suffix() {
    let mut fields = serde_json::Map::new();
    fields.insert("step".to_string(), serde_json::json!("fetch"));
    fields.insert("ms".to_string(), serde_json::json!(24));
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "worker:broadcast".to_string(),
        message: "Step completed".to_string(),
        fields: Some(fields),
        kind: None,
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(plain.contains("Step completed"));
    assert!(plain.contains("step=fetch"));
    assert!(plain.contains("ms=24"));
}

#[test]
fn format_log_splits_compound_scope_with_dim_separator() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "worker:broadcast".to_string(),
        message: "Sleep completed".to_string(),
        fields: None,
        kind: None,
    };
    let rendered = format_log(&log);
    let scope_start = rendered.find("\x1b[38;2;").expect("colored scope present");
    let scope_section = &rendered[scope_start..];
    let dim_idx = scope_section
        .find("\x1b[2m:\x1b[0m")
        .expect("dim `:` separator");
    let prefix_color_end = scope_section[..dim_idx].rfind('m').unwrap();
    let prefix_color = &scope_section[..=prefix_color_end];
    let after_sep = &scope_section[dim_idx + "\x1b[2m:\x1b[0m".len()..];
    let suffix_color_end = after_sep.find('m').unwrap();
    let suffix_color = &after_sep[..=suffix_color_end];
    assert_ne!(prefix_color, suffix_color, "prefix and suffix must differ");
    assert!(strip_ansi(&rendered).contains("worker:broadcast"));
}

#[test]
fn format_log_renders_error_object_as_message_only() {
    let mut fields = serde_json::Map::new();
    fields.insert(
        "error".to_string(),
        serde_json::json!({ "name": "Error", "message": "boom", "stack": "long\nstack\ntrace" }),
    );
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Error,
        scope: "worker:broadcast".to_string(),
        message: "Run failed".to_string(),
        fields: Some(fields),
        kind: None,
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(plain.contains("error=boom"));
    assert!(!plain.contains("stack"));
}

#[test]
fn format_log_skips_build_and_instance_globals() {
    let mut fields = serde_json::Map::new();
    fields.insert("build".to_string(), serde_json::json!("abc"));
    fields.insert("instance".to_string(), serde_json::json!("i1"));
    fields.insert("runId".to_string(), serde_json::json!("t1"));
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "worker:broadcast".to_string(),
        message: "Run started".to_string(),
        fields: Some(fields),
        kind: None,
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(plain.contains("runId=t1"));
    assert!(!plain.contains("build="));
    assert!(!plain.contains("instance="));
}

#[test]
fn format_log_single_line_message_unaffected() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "app".to_string(),
        message: "no newline".to_string(),
        fields: None,
        kind: None,
    };
    let out = format_log(&log);
    assert!(!out.contains('\n'));
    assert!(out.contains("no newline"));
}

#[test]
fn fit_scope_pads_short_scopes() {
    assert_eq!(fit_scope("app"), "app ");
    assert_eq!(fit_scope("up"), "up  ");
}

#[test]
fn fit_scope_keeps_exact_min() {
    assert_eq!(fit_scope("tako"), "tako");
}

#[test]
fn fit_scope_keeps_mid_length() {
    assert_eq!(fit_scope("myservice"), "myservice");
}

#[test]
fn fit_scope_truncates_long_scopes() {
    assert_eq!(
        fit_scope("worker:really-long-workflow-names"),
        "worker:really-long-workflow-nam\u{2026}",
    );
}

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
fn format_lan_block_lists_concrete_routes_and_preserves_paths() {
    // Wildcard routes are excluded from the LAN route list because mDNS
    // can't advertise them — leaving them in would mislead the user into
    // trying a URL their phone can't resolve.
    let lines = format_lan_block(
        &[
            "bun-example.test".to_string(),
            "bun-example.test/bun".to_string(),
            "*.bun-example.test/api/*".to_string(),
        ],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(!plain.contains("LAN mode enabled"));
    assert!(plain.contains("Your app is now available on your local network at these routes"));
    assert!(plain.contains("https://bun-example.local"));
    assert!(plain.contains("https://bun-example.local/bun"));
    assert!(
        !plain.contains("https://*.bun-example.local"),
        "wildcard route should be excluded from the LAN list, got:\n{plain}"
    );
}

#[test]
fn format_lan_block_excludes_external_routes() {
    let lines = format_lan_block(
        &["app.test".to_string(), "tunnel.example.com".to_string()],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(plain.contains("https://app.local"));
    assert!(!plain.contains("tunnel.example.com.local"));
}

#[test]
fn format_lan_block_has_no_reachable_routes_for_external_only_routes() {
    let lines = format_lan_block(
        &["tunnel.example.com".to_string()],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(plain.contains("No routes are reachable on your local network"));
    assert!(!plain.contains("tunnel.example.com.local"));
}

#[test]
fn format_lan_block_shows_unreachable_message_when_only_wildcards() {
    // When every configured route is a wildcard, there is nothing mDNS can
    // advertise, so the header flips to "no routes are reachable" and the
    // warning explains what to do about it.
    let lines = format_lan_block(&["*.app.test".to_string()], "http://192.168.1.2/ca.pem");
    let plain = strip_ansi(&lines.join("\n"));

    assert!(
        plain.contains("No routes are reachable on your local network"),
        "expected unreachable header, got:\n{plain}"
    );
    assert!(
        !plain.contains("Your app is now available on your local network"),
        "available header should be absent when only wildcards exist, got:\n{plain}"
    );
    assert!(plain.contains("Wildcard routes can't be advertised"));
    assert!(plain.contains("e.g. tenant.app.test"));
}

#[test]
fn format_lan_block_warns_below_route_list_with_derived_example() {
    // Warning sits after the full route list (separated by a blank line)
    // with text "! ..." flush-left so the body column matches the URL
    // column above. The remedy example is derived from the app's own
    // wildcard route so the user can copy-paste it into tako.toml.
    let lines = format_lan_block(
        &[
            "demo.test".to_string(),
            "*.demo.test".to_string(),
            "other.test".to_string(),
        ],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(
        plain.contains("Wildcard routes can't be advertised to devices via mDNS"),
        "expected wildcard warning, got:\n{plain}"
    );
    assert!(
        plain.contains("Use non-wildcard routes"),
        "expected 'Use non-wildcard routes' wording, got:\n{plain}"
    );
    assert!(
        plain.contains("e.g. tenant.demo.test"),
        "expected derived example, got:\n{plain}"
    );

    // The warning must sit after every URL in the route list, not inline
    // between URLs — proving it is emitted once at the bottom of the block.
    let last_url_idx = plain
        .rfind("https://other.local")
        .expect("other URL missing");
    let warning_idx = plain
        .find("Wildcard routes can't be advertised")
        .expect("warning missing");
    assert!(
        warning_idx > last_url_idx,
        "expected warning after the full route list, got:\n{plain}"
    );

    // The warning title line is flush-left (`!` at col 0) so "Wildcard"
    // lines up at col 2, matching the "https://" text column of the
    // routes. Find the warning line and check its prefix.
    let warning_line = plain
        .lines()
        .find(|l| l.contains("Wildcard routes can't be advertised"))
        .expect("warning line missing");
    assert!(
        warning_line.starts_with("! Wildcard"),
        "expected warning line to start with '! Wildcard' (no leading indent), got: {warning_line:?}"
    );
}

#[test]
fn format_lan_block_warning_uses_first_wildcard_for_example() {
    // When multiple wildcards exist, derive the example from the first one
    // and emit only a single warning block.
    let lines = format_lan_block(
        &["*.one.test".to_string(), "*.two.test".to_string()],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));

    assert!(plain.contains("e.g. tenant.one.test"));
    assert!(!plain.contains("e.g. tenant.two.test"));
    assert_eq!(
        plain.matches("Wildcard routes can't be advertised").count(),
        1,
        "expected exactly one warning block"
    );
}

#[test]
fn format_lan_block_hints_at_client_isolation_below_qr() {
    // The muted hint under the QR code explains the most common silent
    // failure mode (AP client isolation) so users on guest/coffee-shop
    // Wi-Fi know where to look instead of assuming Tako is broken.
    let lines = format_lan_block(&["demo.test".to_string()], "http://192.168.1.2/ca.pem");
    let plain = strip_ansi(&lines.join("\n"));
    assert!(
        plain.contains("your Wi-Fi may use client isolation"),
        "expected client-isolation hint under QR, got:\n{plain}"
    );
}

#[test]
fn format_lan_block_omits_wildcard_warning_when_none_present() {
    let lines = format_lan_block(
        &["demo.test".to_string(), "demo.test/api".to_string()],
        "http://192.168.1.2/ca.pem",
    );
    let plain = strip_ansi(&lines.join("\n"));
    assert!(!plain.contains("Wildcard routes can't be advertised"));
    assert!(!plain.contains("Use non-wildcard routes"));
}

#[test]
fn format_log_renders_kind_as_divider_with_humanized_label() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "tako".to_string(),
        message: String::new(),
        fields: None,
        kind: Some("lan_mode_enabled".to_string()),
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(
        plain.contains("────"),
        "expected divider rule, got: {plain:?}"
    );
    assert!(
        plain.contains("lan mode enabled"),
        "underscores should become spaces, got: {plain:?}"
    );
    assert!(
        !plain.contains("INFO"),
        "divider should not carry log columns, got: {plain:?}"
    );
    assert!(
        !plain.contains("12:34:56"),
        "divider should omit timestamp, got: {plain:?}"
    );
}

#[test]
fn format_log_without_kind_stays_a_normal_log_line() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "tako".to_string(),
        message: "restarted".to_string(),
        fields: None,
        kind: None,
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(
        !plain.contains("────"),
        "plain log must not render as divider: {plain:?}"
    );
    assert!(plain.contains("INFO"));
    assert!(plain.contains("tako"));
}

#[test]
fn parse_log_line_preserves_kind_from_wire_format() {
    use crate::commands::dev::client::parse_log_line;
    let line = r#"{"ts":1700000000000,"level":"info","scope":"tako","kind":"restarted"}"#;
    let log = parse_log_line(line).expect("kind-only line parses");
    assert_eq!(log.scope, "tako");
    assert_eq!(log.kind.as_deref(), Some("restarted"));
    assert_eq!(log.message, "", "msg is optional when kind is set");
}

#[test]
fn format_log_dims_lan_mode_ip_suffix() {
    let enabled = strip_ansi(&format_log(&ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "tako".to_string(),
        message: "LAN mode enabled (192.168.1.2)".to_string(),
        fields: None,
        kind: None,
    }));
    assert!(enabled.contains("INFO"));
    assert!(enabled.contains("tako"));
    assert!(enabled.contains("LAN mode enabled (192.168.1.2)"));

    let disabled = strip_ansi(&format_log(&ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "tako".to_string(),
        message: "LAN mode disabled".to_string(),
        fields: None,
        kind: None,
    }));
    assert!(disabled.contains("INFO"));
    assert!(disabled.contains("tako"));
    assert!(disabled.contains("LAN mode disabled"));
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
fn format_keymap_has_restart_stop_background() {
    let km = strip_ansi(&format_keymap());
    assert!(km.contains('r'));
    assert!(km.contains("restart"));
    assert!(km.contains("stop"));
    assert!(km.contains('b'));
    assert!(km.contains("background"));
    assert!(!km.contains("quit"));
}

#[test]
fn progress_bar_extremes() {
    let full = strip_ansi(&progress_bar(1.0, 8));
    let empty = strip_ansi(&progress_bar(0.0, 8));
    assert!(full.contains("████████"));
    assert!(empty.contains("⣿⣿⣿⣿⣿⣿⣿⣿"));
}

#[test]
fn vlen_strips_ansi() {
    assert_eq!(vlen(&format!("{DIM}hello{RESET}")), 5);
    assert_eq!(vlen("AB"), 2);
}

#[test]
fn trunc_at_limit() {
    assert_eq!(truncate_str("hello", 10, "…").as_ref(), "hello");
    assert_eq!(measure_text_width(&truncate_str("hello world", 7, "…")), 7);
}

#[test]
fn extract_repo_slug_ssh_url() {
    assert_eq!(
        extract_repo_slug("git@github.com:user/repo.git"),
        "user/repo"
    );
    assert_eq!(
        extract_repo_slug("git@gitlab.com:org/project"),
        "org/project"
    );
}

#[test]
fn extract_repo_slug_https_url() {
    assert_eq!(
        extract_repo_slug("https://github.com/user/repo.git"),
        "user/repo"
    );
    assert_eq!(
        extract_repo_slug("https://github.com/user/repo"),
        "user/repo"
    );
    assert_eq!(
        extract_repo_slug("https://github.com/user/repo/"),
        "user/repo"
    );
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

fn strip_ansi(s: &str) -> String {
    strip_ansi_codes(s).into_owned()
}
