use std::io;

use console::Term;

use super::super::{INDENT, is_interactive, is_pretty, theme_accent, theme_muted, underline};
use super::{
    EscapeAction, format_key_hints_with_enter_action, format_pretty_cancelled_prompt,
    format_pretty_prompt_completion, format_pretty_prompt_header, format_pretty_prompt_hint_line,
    is_wizard_back, prompt_escape_action, wizard_back_error,
};

pub fn select<T>(
    title: &str,
    description: Option<&str>,
    options: Vec<(String, T)>,
) -> io::Result<T> {
    select_with_default(title, description, options, 0)
}

pub fn select_with_default<T>(
    title: &str,
    description: Option<&str>,
    options: Vec<(String, T)>,
    default: usize,
) -> io::Result<T> {
    if !is_interactive() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Missing required selection: {title}. In --ci mode, pass the value via a CLI flag or config."
            ),
        ));
    }

    if options.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "No options available for selection",
        ));
    }

    // Verbose mode: numbered list with simple input.
    // Prompts are NOT wrapped in tracing log lines — they print as plain text.
    if !is_pretty() {
        let labels: Vec<&str> = options.iter().map(|(label, _)| label.as_str()).collect();
        let term = Term::stderr();
        let full_prompt = match description {
            Some(desc) => format!("{title}\n{desc}"),
            None => title.to_string(),
        };
        let index = raw_select(&term, &full_prompt, &labels, &[], None, default, &[])?;
        return Ok(options.into_iter().nth(index).unwrap().1);
    }

    let labels: Vec<&str> = options.iter().map(|(label, _)| label.as_str()).collect();
    let term = Term::stderr();
    let full_prompt = match description {
        Some(desc) => format!("{title}\n{desc}"),
        None => title.to_string(),
    };

    // raw_select clears display and writes completion/cancelled lines
    let index = raw_select(&term, &full_prompt, &labels, &[], None, default, &[])?;
    Ok(options.into_iter().nth(index).unwrap().1)
}

/// Minimal select using crossterm — no cursor, no filter input, just arrow keys.
/// `hints` provides optional muted text after each label (e.g. "detected").
pub(in crate::output) fn raw_select(
    term: &Term,
    prompt: &str,
    labels: &[&str],
    hints: &[&str],
    escape_hint: Option<&str>,
    default: usize,
    footer_lines: &[String],
) -> io::Result<usize> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
        terminal::{self, Clear, ClearType},
    };
    use std::io::Write;

    let mut selected = default;
    let mut out = io::stderr();

    // Print prompt (before raw mode) — diamond style
    for line in format_select_prompt_header_lines(prompt) {
        let _ = term.write_line(&line);
    }

    let key_hints = format_select_key_hints(escape_hint);
    let footer_spacing = usize::from(!footer_lines.is_empty());
    let draw = |sel: usize, out: &mut io::Stderr, key_hints: &str| {
        for line in format_select_option_lines(labels, hints, sel) {
            let _ = write!(out, "{line}\r\n");
        }
        let _ = write!(out, "{key_hints}");
        if !footer_lines.is_empty() {
            let _ = write!(out, "\r\n");
            for line in footer_lines {
                let _ = write!(out, "\r\n{line}");
            }
        }
        let _ = out.flush();
    };

    // Enter raw mode + hide cursor, then draw
    terminal::enable_raw_mode()?;
    crossterm::execute!(out, cursor::Hide)?;
    draw(selected, &mut out, &key_hints);

    let result = loop {
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if selected > 0 {
                        selected -= 1;
                    } else {
                        selected = labels.len() - 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected < labels.len() - 1 {
                        selected += 1;
                    } else {
                        selected = 0;
                    }
                }
                KeyCode::Enter => {
                    break Ok(selected);
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Operation interrupted",
                    ));
                }
                KeyCode::Esc => {
                    if prompt_escape_action(escape_hint.is_some()) == EscapeAction::Back {
                        break Err(wizard_back_error());
                    }
                    continue;
                }
                _ => continue,
            }
            // Move cursor up to first option, clear, and redraw
            // options + key hints line
            let total_draw_lines =
                select_draw_line_count(labels, hints, footer_spacing, footer_lines);
            if total_draw_lines > 1 {
                crossterm::execute!(out, cursor::MoveUp((total_draw_lines - 1) as u16),)?;
            }
            crossterm::execute!(out, cursor::MoveToColumn(0))?;
            for _ in 0..total_draw_lines {
                crossterm::execute!(out, Clear(ClearType::CurrentLine))?;
                let _ = write!(out, "\r\n");
            }
            // Move back up
            crossterm::execute!(
                out,
                cursor::MoveUp(total_draw_lines as u16),
                cursor::MoveToColumn(0),
            )?;
            draw(selected, &mut out, &key_hints);
        }
    };

    // Move cursor below the key hints line so we're on a clean line
    let _ = write!(out, "\r\n");
    let _ = out.flush();

    // Restore terminal
    terminal::disable_raw_mode()?;

    // Clear the select display and write appropriate completion
    if is_pretty() {
        let prompt_lines = prompt.chars().filter(|c| *c == '\n').count() + 1;
        let total =
            prompt_lines + select_draw_line_count(labels, hints, footer_spacing, footer_lines);
        let _ = term.clear_last_lines(total);

        let title = prompt.lines().next().unwrap_or(prompt);
        for line in format_select_completion_lines(title, labels, &result) {
            let _ = term.write_line(&line);
        }
    }

    result
}

fn format_select_prompt_header_lines(prompt: &str) -> Vec<String> {
    let mut lines = prompt.lines();
    let title = lines.next().unwrap_or(prompt);
    let mut formatted = format_pretty_prompt_header(title, None, None);
    formatted.extend(lines.map(format_pretty_prompt_hint_line));
    formatted
}

fn format_select_key_hints(escape_hint: Option<&str>) -> String {
    format_key_hints_with_enter_action(escape_hint, "select")
}

fn format_select_option_lines(labels: &[&str], hints: &[&str], selected: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let hint = hints.get(i).filter(|h| !h.is_empty());
        let mut line = if i == selected {
            format!("{INDENT}{} {}", theme_accent("→"), underline(label))
        } else {
            format!("{INDENT}{INDENT}{label}")
        };
        if let Some(h) = hint {
            line.push(' ');
            line.push_str(&theme_muted(format!("({h})")));
        }
        lines.push(line);
    }
    lines
}

fn select_draw_line_count(
    labels: &[&str],
    hints: &[&str],
    footer_spacing: usize,
    footer_lines: &[String],
) -> usize {
    format_select_option_lines(labels, hints, 0).len() + 1 + footer_spacing + footer_lines.len()
}

fn format_select_completion_lines(
    title: &str,
    labels: &[&str],
    result: &io::Result<usize>,
) -> Vec<String> {
    match result {
        Ok(idx) => format_pretty_prompt_completion(title, labels[*idx]),
        Err(e) if e.kind() == io::ErrorKind::Interrupted && !is_wizard_back(e) => {
            format_pretty_cancelled_prompt(title)
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::is_interactive;
    use super::*;

    #[test]
    fn select_errors_in_non_tty_context() {
        if is_interactive() {
            return;
        }
        let err = select("Pick one", None, vec![("server-a".to_string(), 1)]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn select_completion_keeps_cancelled_completion() {
        let result = Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Operation interrupted",
        ));

        let lines = format_select_completion_lines(
            "Select setting",
            &["DNS wildcard certificates"],
            &result,
        );

        assert_eq!(lines, vec!["◇ Select setting".to_string(), String::new()]);
    }

    #[test]
    fn select_completion_omits_back_completion() {
        let result = Err(io::Error::new(io::ErrorKind::Interrupted, "wizard_back"));

        let lines = format_select_completion_lines(
            "Select setting",
            &["DNS wildcard certificates"],
            &result,
        );

        assert!(lines.is_empty());
    }

    #[test]
    fn select_prompt_description_is_muted_below_title() {
        let lines = format_select_prompt_header_lines(
            "Source IP mode\nUse Direct traffic unless every request reaches Tako through a trusted proxy.",
        );

        assert_eq!(
            lines,
            vec![
                "◆ Source IP mode".to_string(),
                "  Use Direct traffic unless every request reaches Tako through a trusted proxy."
                    .to_string(),
            ],
        );
    }

    #[test]
    fn select_key_hints_use_select_action() {
        assert_eq!(format_select_key_hints(None), "  enter select".to_string());
        assert_eq!(
            format_select_key_hints(Some("back")),
            "  esc back · enter select".to_string(),
        );
    }

    #[test]
    fn select_options_render_labels_and_hints_only() {
        let lines =
            format_select_option_lines(&["Direct traffic", "PROXY protocol"], &["", "detected"], 0);

        assert_eq!(
            lines,
            vec![
                "  → Direct traffic".to_string(),
                "    PROXY protocol (detected)".to_string(),
            ],
        );
    }
}
