mod confirm;
pub mod select;
pub mod text_field;
mod wizard;

pub use confirm::{confirm, confirm_with_description};
pub use select::select;
pub use text_field::{TextField, password_field};
pub use wizard::Wizard;

use std::io;

use super::{
    DIAMOND_FILLED, DIAMOND_OUTLINED, INDENT, OPERATION_CANCELLED, emit, theme_accent, theme_dim,
    theme_error, theme_muted, theme_warning,
};

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/// Check if an error signals "go back" (ESC pressed in a wizard prompt).
pub fn is_wizard_back(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted && err.to_string() == "wizard_back"
}

fn is_operation_cancelled(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted && !is_wizard_back(err)
}

pub fn is_operation_cancelled_error(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(is_operation_cancelled)
}

pub fn operation_cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, OPERATION_CANCELLED)
}

const SILENT_EXIT: &str = "silent_exit";

/// Returns an error that signals the process should exit with failure but not print any message.
/// Use when the error was already displayed to the user (e.g. in a task tree).
pub fn silent_exit_error() -> io::Error {
    io::Error::other(SILENT_EXIT)
}

fn is_silent_exit(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Other && err.to_string() == SILENT_EXIT
}

pub fn is_silent_exit_error(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<io::Error>().is_some_and(is_silent_exit)
}

fn format_operation_cancelled_lines() -> Vec<String> {
    vec![theme_error(OPERATION_CANCELLED)]
}

pub fn operation_cancelled() {
    for line in format_operation_cancelled_lines() {
        emit(&line);
    }
}

fn wizard_back_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "wizard_back")
}

#[derive(Debug, PartialEq, Eq)]
enum EscapeAction {
    Ignore,
    Back,
}

fn prompt_escape_action(show_back: bool) -> EscapeAction {
    if show_back {
        EscapeAction::Back
    } else {
        EscapeAction::Ignore
    }
}

// ---------------------------------------------------------------------------
// Prompt format helpers
// ---------------------------------------------------------------------------

fn format_pretty_prompt_warning_line(message: &str) -> String {
    format!("{INDENT}{}", theme_warning(message))
}

fn format_pretty_prompt_error_line(message: &str) -> String {
    format!("{INDENT}{}", theme_error(message))
}

fn format_pretty_prompt_header(
    label: &str,
    warning: Option<&str>,
    error: Option<&str>,
) -> Vec<String> {
    let has_error = error.is_some();
    let diamond = if has_error {
        theme_error(DIAMOND_FILLED)
    } else {
        theme_accent(DIAMOND_FILLED)
    };
    let label = if has_error {
        theme_error(label)
    } else {
        theme_accent(label)
    };
    let mut lines = vec![format!("{diamond} {label}")];
    if let Some(warning_text) = warning {
        lines.push(format_pretty_prompt_warning_line(warning_text));
    }
    lines
}

fn format_pretty_prompt_input_prefix(active: bool) -> String {
    format_pretty_prompt_input_prefix_for_status(active, false)
}

fn format_pretty_prompt_input_prefix_for_status(active: bool, has_error: bool) -> String {
    let marker = if active {
        if has_error {
            theme_error("›")
        } else {
            theme_accent("›")
        }
    } else {
        theme_muted("›")
    };
    format!("{INDENT}{marker} ")
}

fn pretty_prompt_input_column(active: bool, has_error: bool) -> u16 {
    console::measure_text_width(&format_pretty_prompt_input_prefix_for_status(
        active, has_error,
    )) as u16
}

fn format_pretty_prompt_value_line(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        format!("{INDENT}{value}")
    }
}

fn format_pretty_prompt_completion(label: &str, value: &str) -> Vec<String> {
    let done_diamond = theme_muted(DIAMOND_OUTLINED);
    let done_label = theme_muted(label);
    vec![
        format!("{done_diamond} {done_label}"),
        format_pretty_prompt_value_line(value),
        String::new(),
    ]
}

fn format_pretty_text_prompt_completion(
    label: &str,
    warning: Option<&str>,
    value: &str,
) -> Vec<String> {
    let done_diamond = theme_muted(DIAMOND_OUTLINED);
    let done_label = theme_muted(label);
    let mut lines = vec![format!("{done_diamond} {done_label}")];
    if let Some(warning_text) = warning {
        lines.push(format_pretty_prompt_warning_line(warning_text));
    }
    lines.push(format_pretty_prompt_value_line(value));
    lines.push(String::new());
    lines
}

/// Keyboard shortcut hints shown below the active prompt input line.
/// `show_back` adds "esc go back" for non-first wizard steps.
pub fn format_key_hints(show_back: bool) -> String {
    let escape_action = show_back.then_some("back");
    format_key_hints_with_enter_action(escape_action, "submit")
}

fn format_key_hints_with_enter_action(escape_action: Option<&str>, enter_action: &str) -> String {
    let dot = theme_dim("·");
    let mut parts = Vec::new();
    if let Some(action) = escape_action {
        parts.push(format!("{} {}", theme_muted("esc"), theme_dim(action)));
    }
    parts.push(format!(
        "{} {}",
        theme_muted("enter"),
        theme_dim(enter_action)
    ));
    format!("{INDENT}{}", parts.join(&format!(" {dot} ")))
}

fn format_pretty_prompt_hint_line(message: &str) -> String {
    format!("{INDENT}{}", theme_muted(message))
}

fn strikethrough<D: std::fmt::Display>(value: D) -> String {
    if super::should_colorize() {
        format!("\x1b[9m{value}\x1b[29m")
    } else {
        value.to_string()
    }
}

fn format_pretty_confirm_label(label: &str, default: bool, active: bool) -> String {
    let diamond = if active {
        theme_accent(DIAMOND_FILLED)
    } else {
        theme_muted(DIAMOND_OUTLINED)
    };
    let label = if active {
        theme_accent(label)
    } else {
        theme_muted(label)
    };
    if active {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        format!("{diamond} {label} {}", theme_muted(hint))
    } else {
        format!("{diamond} {label}")
    }
}

fn format_pretty_confirm_prompt_with_description(
    label: &str,
    description: Option<&str>,
    default: bool,
) -> Vec<String> {
    let mut lines = vec![format_pretty_confirm_label(label, default, true)];
    if let Some(description) = description {
        lines.push(format_pretty_prompt_hint_line(description));
    }
    lines.push(
        format_pretty_prompt_input_prefix(true)
            .trim_end()
            .to_string(),
    );
    lines
}

fn format_pretty_confirm_completion(label: &str, default: bool, value: &str) -> Vec<String> {
    vec![
        format_pretty_confirm_label(label, default, false),
        format_pretty_prompt_value_line(value),
        String::new(),
    ]
}

/// Collapsed prompt summary shown after Ctrl-C.
///
/// This intentionally strips prompt chrome like warnings, hints, input echoes,
/// and confirm defaults so cancelled prompts all resolve to the same muted line.
fn format_pretty_cancelled_prompt(label: &str) -> Vec<String> {
    let done_diamond = theme_muted(DIAMOND_OUTLINED);
    let done_label = theme_muted(strikethrough(label));
    vec![format!("{done_diamond} {done_label}"), String::new()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_prompt_header_places_warning_below_label() {
        let lines = format_pretty_prompt_header(
            "Application name",
            Some("Name cannot be changed after the first deployment."),
            None,
        );

        assert_eq!(
            lines,
            vec![
                "◆ Application name".to_string(),
                "  Name cannot be changed after the first deployment.".to_string(),
            ]
        );
    }

    #[test]
    fn pretty_prompt_header_marks_error_without_rendering_error_text() {
        let lines = format_pretty_prompt_header("Passphrase", None, Some("Invalid passphrase"));

        assert_eq!(lines, vec!["◆ Passphrase".to_string()]);
    }

    #[test]
    fn pretty_prompt_error_line_is_indented_without_icon() {
        assert_eq!(
            format_pretty_prompt_error_line("Invalid passphrase"),
            "  Invalid passphrase".to_string(),
        );
    }

    #[test]
    fn pretty_prompt_hint_line_is_indented() {
        assert_eq!(
            format_pretty_prompt_hint_line("Cloudflare API token"),
            "  Cloudflare API token".to_string()
        );
    }

    #[test]
    fn pretty_prompt_input_prefix_uses_chevron() {
        assert_eq!(format_pretty_prompt_input_prefix(true), "  › ".to_string());
        assert_eq!(format_pretty_prompt_input_prefix(false), "  › ".to_string());
    }

    #[test]
    fn pretty_confirm_prompt_keeps_default_choice_on_label_line() {
        let lines =
            format_pretty_confirm_prompt_with_description("Overwrite configuration?", None, false);
        assert_eq!(
            lines,
            vec![
                "◆ Overwrite configuration? [y/N]".to_string(),
                "  ›".to_string(),
            ]
        );
    }

    #[test]
    fn pretty_confirm_prompt_indents_description_inside_prompt_body() {
        let lines = format_pretty_confirm_prompt_with_description(
            "Use iCloud Keychain?",
            Some("Stores the key in your macOS login keychain."),
            false,
        );
        assert_eq!(
            lines,
            vec![
                "◆ Use iCloud Keychain? [y/N]".to_string(),
                "  Stores the key in your macOS login keychain.".to_string(),
                "  ›".to_string(),
            ]
        );
    }

    #[test]
    fn pretty_confirm_completion_aligns_answer_under_label_without_chevron() {
        let lines = format_pretty_confirm_completion("Overwrite configuration?", false, "no");
        assert_eq!(
            lines,
            vec![
                "◇ Overwrite configuration?".to_string(),
                "  no".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn pretty_prompt_completion_shows_value_on_separate_line() {
        let lines = format_pretty_prompt_completion("Runtime", "bun");
        assert_eq!(
            lines,
            vec!["◇ Runtime".to_string(), "  bun".to_string(), String::new(),]
        );
    }

    #[test]
    fn pretty_cancelled_prompt_uses_cancelled_summary_line() {
        let lines = format_pretty_cancelled_prompt("Runtime");
        assert_eq!(lines, vec!["◇ Runtime".to_string(), String::new()]);
    }

    #[test]
    fn pretty_cancelled_confirm_omits_default_choice_hint() {
        let lines = format_pretty_cancelled_prompt("Overwrite configuration?");
        assert_eq!(
            lines,
            vec!["◇ Overwrite configuration?".to_string(), String::new()]
        );
    }

    #[test]
    fn operation_cancelled_error_uses_interrupted_kind() {
        let err = operation_cancelled_error();
        assert!(is_operation_cancelled(&err));
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(err.to_string(), "Operation cancelled");
    }

    #[test]
    fn silent_exit_error_is_detected_by_is_silent_exit_error() {
        let err = silent_exit_error();
        assert!(is_silent_exit_error(&err));
    }

    #[test]
    fn is_silent_exit_error_rejects_other_errors() {
        let other = std::io::Error::other("some other error");
        assert!(!is_silent_exit_error(&other));
        let cancelled = operation_cancelled_error();
        assert!(!is_silent_exit_error(&cancelled));
    }

    #[test]
    fn format_operation_cancelled_lines_output() {
        assert_eq!(
            format_operation_cancelled_lines(),
            vec![theme_error(OPERATION_CANCELLED)]
        );
    }

    #[test]
    fn prompt_escape_ignores_when_back_hint_is_hidden() {
        assert_eq!(prompt_escape_action(false), EscapeAction::Ignore);
    }

    #[test]
    fn prompt_escape_goes_back_when_back_hint_is_visible() {
        assert_eq!(prompt_escape_action(true), EscapeAction::Back);
    }

    #[test]
    fn pretty_text_prompt_completion_keeps_warning_and_value_on_separate_lines() {
        let lines = format_pretty_text_prompt_completion(
            "Application name",
            Some("Name cannot be changed after the first deployment."),
            "my-app",
        );

        assert_eq!(
            lines,
            vec![
                "◇ Application name".to_string(),
                "  Name cannot be changed after the first deployment.".to_string(),
                "  my-app".to_string(),
                String::new(),
            ]
        );
    }
}
