use super::super::super::is_interactive;
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
fn prompt_validation_marker_offset_starts_from_label_line() {
    assert_eq!(prompt_validation_marker_offset(None), 2);
    assert_eq!(prompt_validation_marker_offset(Some("warning")), 3);
}
