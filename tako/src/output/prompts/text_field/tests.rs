use super::super::super::is_interactive;
use super::raw::filter_suggestions;
use super::validation::prompt_validation_marker_offset;
use super::*;

#[test]
fn text_field_uses_default_in_non_tty_context() {
    if is_interactive() {
        return;
    }
    let value = TextField::new("Server host")
        .default_opt(Some("localhost"))
        .prompt()
        .unwrap();
    assert_eq!(value, "localhost");
}

#[test]
fn text_field_without_default_errors_in_non_tty_context() {
    if is_interactive() {
        return;
    }
    let err = TextField::new("Server host")
        .default_opt(None)
        .prompt()
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn text_field_with_suggestions_uses_default_in_non_tty_context() {
    if is_interactive() {
        return;
    }
    let suggestions = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let value = TextField::new("Server host")
        .with_default("localhost")
        .suggestions(&suggestions)
        .prompt()
        .unwrap();
    assert_eq!(value, "localhost");
}

#[test]
fn text_field_with_suggestions_without_default_errors_in_non_tty_context() {
    if is_interactive() {
        return;
    }
    let suggestions = vec!["localhost".to_string()];
    let err = TextField::new("Server host")
        .suggestions(&suggestions)
        .prompt()
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn filter_suggestions_preserves_input_order() {
    let suggestions = vec![
        "related-2".to_string(),
        "related-1".to_string(),
        "global-a".to_string(),
        "related-2".to_string(),
    ];
    let filtered = filter_suggestions(&suggestions, "");
    assert_eq!(
        filtered,
        vec![
            "related-2".to_string(),
            "related-1".to_string(),
            "global-a".to_string()
        ]
    );
}

#[test]
fn filter_suggestions_matches_case_insensitive_substring() {
    let suggestions = vec![
        "Prod-EU".to_string(),
        "staging-us".to_string(),
        "prod-us".to_string(),
    ];
    let filtered = filter_suggestions(&suggestions, "PROD");
    assert_eq!(filtered, vec!["Prod-EU".to_string(), "prod-us".to_string()]);
}

#[test]
fn text_input_preserves_newlines_when_multiline_paste_is_allowed() {
    assert_eq!(
        super::raw::normalize_paste_for_text_input("line 1\r\nline 2\n", true),
        "line 1\nline 2\n"
    );
}

#[test]
fn text_input_flattens_newlines_when_multiline_paste_is_not_allowed() {
    assert_eq!(
        super::raw::normalize_paste_for_text_input("line 1\r\nline 2\n", false),
        "line 1 line 2 "
    );
}

#[test]
fn text_input_inserts_paste_at_cursor() {
    let mut chars: Vec<char> = "abef".chars().collect();
    let mut pos = 2;

    super::raw::insert_text_at_cursor(&mut chars, &mut pos, "cd");

    assert_eq!(chars.iter().collect::<String>(), "abcdef");
    assert_eq!(pos, 4);
}

#[test]
fn password_input_masks_short_values_by_length() {
    assert_eq!(super::raw::password_display_value(&[]), "");
    assert_eq!(
        super::raw::password_display_value(&"secret".chars().collect::<Vec<_>>()),
        "••••••"
    );
}

#[test]
fn password_input_caps_long_mask_with_summary() {
    let input = "a".repeat(30);

    assert_eq!(
        super::raw::password_display_value(&input.chars().collect::<Vec<_>>()),
        "••••••••••••••••••••••••… (30 chars)"
    );
}

#[test]
fn password_input_summary_counts_multiline_values() {
    let input = "a".repeat(24) + "\nb";

    assert_eq!(
        super::raw::password_display_value(&input.chars().collect::<Vec<_>>()),
        "••••••••••••••••••••••••… (26 chars, 2 lines)"
    );
}

#[test]
fn prompt_validation_marker_offset_starts_from_label_line() {
    assert_eq!(prompt_validation_marker_offset(None), 2);
    assert_eq!(prompt_validation_marker_offset(Some("warning")), 3);
}
