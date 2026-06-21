use std::io;

use super::super::super::{ACCENT, theme_dim, theme_error, theme_muted};
use super::super::{EscapeAction, prompt_escape_action, wizard_back_error};

pub(super) struct RawTextInputOptions<'a> {
    pub(super) initial: Option<&'a str>,
    pub(super) suggestions: &'a [String],
    pub(super) password: bool,
    pub(super) placeholder_override: Option<&'a str>,
    pub(super) required: bool,
    pub(super) trimmed: bool,
    pub(super) multiline_paste: bool,
    pub(super) use_separator: bool,
    pub(super) error: bool,
    pub(super) show_back: bool,
}

pub(super) fn raw_text_input(prompt: &str, options: RawTextInputOptions<'_>) -> io::Result<String> {
    use crossterm::{
        cursor,
        event::{
            self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent,
            KeyModifiers,
        },
        terminal::{self, Clear, ClearType},
    };
    use std::io::Write;

    let mut out = io::stderr();
    let RawTextInputOptions {
        initial,
        suggestions,
        password,
        placeholder_override,
        required,
        trimmed,
        multiline_paste,
        use_separator,
        error,
        show_back,
    } = options;

    let mut buf: Vec<char> = initial.unwrap_or("").chars().collect();
    let mut pos: usize = buf.len();
    let mut suggestion_idx: Option<usize> = None;

    let placeholder: Option<String> = if initial.is_some() {
        None
    } else if password && !error {
        Some("••••••".to_string())
    } else if let Some(ph) = placeholder_override {
        Some(ph.to_string())
    } else {
        suggestions.first().cloned()
    };

    let separator = if error {
        theme_error("›")
    } else {
        theme_muted("›")
    };

    let sep_display_width: usize = if use_separator { 3 } else { 0 };

    let inline_suffix = |buf: &[char]| -> String {
        if buf.is_empty() || suggestions.is_empty() || password {
            return String::new();
        }
        let current: String = buf.iter().collect();
        let lower = current.to_lowercase();
        for s in suggestions {
            if s.to_lowercase().starts_with(&lower) && s.len() > current.len() {
                return s.chars().skip(current.chars().count()).collect();
            }
        }
        String::new()
    };

    let draw = |buf: &[char],
                pos: usize,
                out: &mut io::Stderr,
                password: bool,
                placeholder: &Option<String>,
                suffix: &str| {
        let _ = write!(out, "\r");
        let _ = crossterm::execute!(*out, Clear(ClearType::CurrentLine));
        if buf.is_empty() {
            if let Some(ph) = placeholder {
                let dimmed = if error {
                    theme_error(ph)
                } else {
                    theme_dim(ph)
                };
                if use_separator {
                    let _ = write!(out, "{prompt} {separator} {dimmed}");
                } else {
                    let _ = write!(out, "{prompt}{dimmed}");
                }
            } else if use_separator {
                let _ = write!(out, "{prompt} {separator} ");
            } else {
                let _ = write!(out, "{prompt}");
            }
        } else {
            let display = if password {
                format_password_display(buf, error)
            } else {
                let display: String = buf.iter().collect();
                if error { theme_error(display) } else { display }
            };
            if use_separator {
                let _ = write!(out, "{prompt} {separator} {display}");
            } else {
                let _ = write!(out, "{prompt}{display}");
            }
            if !suffix.is_empty() && pos == buf.len() {
                let suffix = if error {
                    theme_error(suffix)
                } else {
                    theme_dim(suffix)
                };
                let _ = write!(out, "{suffix}");
            }
        }
        let prompt_width = console::measure_text_width(prompt);
        let cursor_offset = if password {
            console::measure_text_width(&password_display_value(buf))
        } else {
            buf[..pos].iter().collect::<String>().len()
        };
        let col = prompt_width + sep_display_width + cursor_offset;
        let _ = crossterm::execute!(*out, cursor::MoveToColumn(col as u16));
        let _ = out.flush();
    };

    let accept_inline = |buf: &mut Vec<char>, pos: &mut usize, suggestions: &[String]| -> bool {
        if buf.is_empty() || suggestions.is_empty() {
            return false;
        }
        let current: String = buf.iter().collect();
        let lower = current.to_lowercase();
        if let Some(sugg) = suggestions
            .iter()
            .find(|s| s.to_lowercase().starts_with(&lower) && s.len() > current.len())
        {
            *buf = sugg.chars().collect();
            *pos = buf.len();
            true
        } else {
            false
        }
    };

    terminal::enable_raw_mode()?;
    let _ = crossterm::execute!(out, cursor::Show, EnableBracketedPaste);

    let (cr, cg, cb) = ACCENT;
    let _ = write!(out, "\x1b]12;rgb:{cr:02x}/{cg:02x}/{cb:02x}\x1b\\");
    let _ = out.flush();

    let suf = inline_suffix(&buf);
    draw(&buf, pos, &mut out, password, &placeholder, &suf);

    let result = loop {
        match event::read()? {
            Event::Paste(pasted) => {
                let mut text = normalize_paste_for_text_input(&pasted, multiline_paste);
                if trimmed && buf[..pos].iter().all(|ch| ch.is_whitespace()) {
                    text = text.trim_start().to_string();
                }
                insert_text_at_cursor(&mut buf, &mut pos, &text);
                suggestion_idx = None;
                let suf = inline_suffix(&buf);
                draw(&buf, pos, &mut out, password, &placeholder, &suf);
            }
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => {
                match code {
                    KeyCode::Enter => {
                        let mut result: String = buf.iter().collect();
                        if trimmed {
                            result = result.trim().to_string();
                        }
                        if required && result.is_empty() {
                            continue;
                        }
                        break Ok(result);
                    }
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        break Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "Operation interrupted",
                        ));
                    }
                    KeyCode::Esc => {
                        if prompt_escape_action(show_back) == EscapeAction::Back {
                            break Err(wizard_back_error());
                        }
                        continue;
                    }
                    KeyCode::Char(c)
                        if !modifiers.contains(KeyModifiers::CONTROL)
                            && !modifiers.contains(KeyModifiers::ALT) =>
                    {
                        if trimmed
                            && c.is_whitespace()
                            && buf[..pos].iter().all(|ch| ch.is_whitespace())
                        {
                            continue;
                        }
                        buf.insert(pos, c);
                        pos += 1;
                        suggestion_idx = None;
                    }
                    KeyCode::Backspace
                        if (modifiers.contains(KeyModifiers::SUPER)
                            || modifiers.contains(KeyModifiers::ALT))
                            && pos > 0 =>
                    {
                        let old_pos = pos;
                        while pos > 0 && buf[pos - 1].is_whitespace() {
                            pos -= 1;
                        }
                        while pos > 0 && !buf[pos - 1].is_whitespace() {
                            pos -= 1;
                        }
                        buf.drain(pos..old_pos);
                        suggestion_idx = None;
                    }
                    KeyCode::Backspace if pos > 0 => {
                        pos -= 1;
                        buf.remove(pos);
                        suggestion_idx = None;
                    }
                    KeyCode::Delete if pos < buf.len() => {
                        buf.remove(pos);
                    }
                    KeyCode::Left
                        if modifiers.contains(KeyModifiers::SUPER)
                            || modifiers.contains(KeyModifiers::ALT) =>
                    {
                        while pos > 0 && buf[pos - 1].is_whitespace() {
                            pos -= 1;
                        }
                        while pos > 0 && !buf[pos - 1].is_whitespace() {
                            pos -= 1;
                        }
                    }
                    KeyCode::Left => {
                        pos = pos.saturating_sub(1);
                    }
                    KeyCode::Right
                        if modifiers.contains(KeyModifiers::SUPER)
                            || modifiers.contains(KeyModifiers::ALT) =>
                    {
                        while pos < buf.len() && !buf[pos].is_whitespace() {
                            pos += 1;
                        }
                        while pos < buf.len() && buf[pos].is_whitespace() {
                            pos += 1;
                        }
                    }
                    KeyCode::Right => {
                        if pos < buf.len() {
                            pos += 1;
                        } else {
                            accept_inline(&mut buf, &mut pos, suggestions);
                            suggestion_idx = None;
                        }
                    }
                    KeyCode::Home | KeyCode::Char('a')
                        if modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        pos = 0;
                    }
                    KeyCode::End | KeyCode::Char('e')
                        if modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        pos = buf.len();
                    }
                    KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
                        buf.truncate(pos);
                    }
                    KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                        buf.drain(..pos);
                        pos = 0;
                    }
                    KeyCode::Tab | KeyCode::BackTab if !suggestions.is_empty() && !password => {
                        let current: String = buf.iter().collect();
                        let needle = current.to_lowercase();
                        let matches: Vec<&String> = suggestions
                            .iter()
                            .filter(|s| needle.is_empty() || s.to_lowercase().contains(&needle))
                            .collect();
                        if !matches.is_empty() {
                            let idx = match suggestion_idx {
                                Some(i) => {
                                    if code == KeyCode::BackTab {
                                        if i == 0 { matches.len() - 1 } else { i - 1 }
                                    } else {
                                        (i + 1) % matches.len()
                                    }
                                }
                                None => 0,
                            };
                            suggestion_idx = Some(idx);
                            buf = matches[idx].chars().collect();
                            pos = buf.len();
                        }
                    }
                    _ => {}
                }
                let suf = inline_suffix(&buf);
                draw(&buf, pos, &mut out, password, &placeholder, &suf);
            }
            _ => {}
        }
    };

    if crate::output::cursor::is_cursor_globally_hidden() {
        let _ = crossterm::execute!(out, crossterm::cursor::Hide);
    }
    let _ = crossterm::execute!(out, DisableBracketedPaste);
    terminal::disable_raw_mode()?;

    let _ = write!(out, "\x1b]112\x1b\\");
    let _ = write!(out, "\r\n");
    let _ = out.flush();

    result
}

pub(super) fn normalize_paste_for_text_input(input: &str, multiline: bool) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    if multiline {
        normalized
    } else {
        normalized.replace('\n', " ")
    }
}

pub(super) fn insert_text_at_cursor(buf: &mut Vec<char>, pos: &mut usize, text: &str) {
    if text.is_empty() {
        return;
    }

    let inserted: Vec<char> = text.chars().collect();
    buf.splice(*pos..*pos, inserted.iter().copied());
    *pos += inserted.len();
}

const PASSWORD_MASK_LIMIT: usize = 24;

pub(super) fn password_display_value(buf: &[char]) -> String {
    match buf.len() {
        0 => String::new(),
        len if len <= PASSWORD_MASK_LIMIT => "•".repeat(len),
        len => {
            let line_count = buf.iter().filter(|ch| **ch == '\n').count() + 1;
            let summary = if line_count > 1 {
                format!("{len} chars, {line_count} lines")
            } else {
                format!("{len} chars")
            };
            format!("{}… ({summary})", "•".repeat(PASSWORD_MASK_LIMIT))
        }
    }
}

fn format_password_display(buf: &[char], error: bool) -> String {
    let display = password_display_value(buf);
    if buf.len() <= PASSWORD_MASK_LIMIT {
        return if error {
            theme_error(display)
        } else {
            display.to_string()
        };
    }

    let Some((mask, summary)) = display.split_once(' ') else {
        return if error { theme_error(display) } else { display };
    };
    let mask = if error {
        theme_error(mask)
    } else {
        mask.to_string()
    };
    format!("{mask} {}", theme_dim(summary))
}

#[cfg(test)]
pub(super) fn filter_suggestions(suggestions: &[String], current_input: &str) -> Vec<String> {
    let needle = current_input.to_lowercase();
    let mut filtered = Vec::new();

    for candidate in suggestions {
        if !needle.is_empty() && !candidate.to_lowercase().contains(&needle) {
            continue;
        }
        if !filtered.iter().any(|existing| existing == candidate) {
            filtered.push(candidate.clone());
        }
    }

    filtered
}
