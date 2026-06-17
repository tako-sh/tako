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
fn client_connected_block_renders_other_clients_as_special_block() {
    let rendered = strip_ansi(&client_connected_block(false, 60950));
    assert_eq!(rendered, "──── Client 60950 connected ────");
    assert!(!rendered.contains("DEBUG"));
    assert!(!rendered.contains("tako"));
    assert!(!rendered.contains("23:02:44"));
}

#[test]
fn client_connected_block_keeps_self_connection_label_short() {
    let rendered = strip_ansi(&client_connected_block(true, 60950));
    assert_eq!(rendered, "──── connected ────");
}

#[test]
fn trunc_at_limit() {
    assert_eq!(truncate_str("hello", 10, "…").as_ref(), "hello");
    assert_eq!(measure_text_width(&truncate_str("hello world", 7, "…")), 7);
}
