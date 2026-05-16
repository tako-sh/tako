use super::*;

#[test]
fn verbose_round_trip() {
    set_verbose(false);
    assert!(!is_verbose());
    set_verbose(true);
    assert!(is_verbose());
}

#[test]
fn strong_returns_plain_in_test() {
    assert_eq!(strong("production"), "production");
}

#[test]
fn format_elapsed_omits_below_threshold() {
    assert_eq!(format_elapsed(Duration::from_millis(50)), "");
    assert_eq!(format_elapsed(Duration::from_millis(99)), "");
}

#[test]
fn format_elapsed_one_decimal_under_ten_seconds() {
    assert_eq!(format_elapsed(Duration::from_millis(3200)), "3.2s");
    assert_eq!(format_elapsed(Duration::from_millis(100)), "0.1s");
}

#[test]
fn format_elapsed_whole_seconds_under_sixty() {
    assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
    assert_eq!(format_elapsed(Duration::from_secs(10)), "10s");
}

#[test]
fn format_elapsed_minutes_and_seconds() {
    assert_eq!(format_elapsed(Duration::from_secs(125)), "2m5s");
    assert_eq!(format_elapsed(Duration::from_secs(60)), "1m0s");
}

#[test]
fn theme_accent_returns_plain_in_test() {
    assert_eq!(theme_accent("hello"), "hello");
    assert_eq!(theme_success("ok"), "ok");
    assert_eq!(theme_warning("warn"), "warn");
    assert_eq!(theme_error("fail"), "fail");
}

#[test]
fn warning_formatters_render_plain_text_in_tests() {
    assert_eq!(
        format_warning_full_line("One-time sudo required"),
        "┃ One-time sudo required"
    );
    assert_eq!(
        format_warning_bullet_line("Configure local DNS for *.test"),
        "┃ • Configure local DNS for *.test"
    );
}

#[test]
fn format_dry_run_skip_plain_uses_ci_friendly_text() {
    assert_eq!(
        format_dry_run_skip_plain("Add server test-srv"),
        "⏭ Add server test-srv (dry-run)"
    );
}

#[test]
fn ci_round_trip() {
    set_ci(false);
    assert!(!is_ci());
    set_ci(true);
    assert!(is_ci());
    set_ci(false);
}

#[test]
fn format_size_uses_expected_units() {
    assert_eq!(format_size(0), "0 bytes");
    assert_eq!(format_size(999), "999 bytes");
    assert_eq!(format_size(1024), "1.00 KB");
    assert_eq!(format_size(1536), "1.50 KB");
    assert_eq!(format_size(1024 * 1024), "1.00 MB");
    assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
}

#[test]
fn format_elapsed_trace_always_shows_value() {
    assert_eq!(format_elapsed_trace(Duration::from_millis(3)), "(3ms)");
    assert_eq!(format_elapsed_trace(Duration::from_millis(50)), "(50ms)");
    assert_eq!(format_elapsed_trace(Duration::from_millis(999)), "(999ms)");
    assert_eq!(format_elapsed_trace(Duration::from_millis(1200)), "(1.2s)");
    assert_eq!(format_elapsed_trace(Duration::from_secs(42)), "(42s)");
    assert_eq!(format_elapsed_trace(Duration::from_secs(125)), "(2m5s)");
}

#[test]
fn format_error_block_renders_plain_wrapped_lines_without_prompt_chrome() {
    assert_eq!(format_error_block("Invalid value"), "Invalid value");
}

#[test]
fn format_elapsed_always_shows_sub_100ms() {
    assert_eq!(format_elapsed_always(Duration::from_millis(80)), "0.1s");
    assert_eq!(format_elapsed_always(Duration::from_millis(50)), "0.1s");
    assert_eq!(format_elapsed_always(Duration::from_millis(3200)), "3.2s");
}

#[test]
fn format_elapsed_always_hides_near_zero() {
    assert_eq!(format_elapsed_always(Duration::from_millis(0)), "");
    assert_eq!(format_elapsed_always(Duration::from_millis(49)), "");
}

#[test]
fn format_success_elapsed_line_uses_single_space_before_elapsed() {
    assert_eq!(
        format_success_elapsed_line("Connection successful", Duration::from_secs(12)),
        "✔ Connection successful 12s"
    );
}

#[test]
fn format_success_elapsed_line_omits_fast_elapsed() {
    assert_eq!(
        format_success_elapsed_line("Connection successful", Duration::from_millis(50)),
        "✔ Connection successful"
    );
}
