use super::*;

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
