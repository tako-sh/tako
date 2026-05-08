use std::io;

use super::super::{
    ACCENT, bold, is_interactive, is_pretty, theme_accent, theme_dim, theme_error, theme_muted,
    theme_warning,
};
use super::wizard_back_error;

pub fn password_field(prompt: &str) -> io::Result<String> {
    TextField::new(prompt).password().prompt()
}

#[derive(Clone)]
pub struct TextField<'a> {
    pub(super) label: &'a str,
    pub(super) warning: Option<&'a str>,
    hint: Option<&'a str>,
    footer_lines: Vec<String>,
    placeholder: Option<&'a str>,
    required: bool,
    trimmed: bool,
    default: Option<&'a str>,
    suggestions: &'a [String],
    password: bool,
    show_back: bool,
}

impl<'a> TextField<'a> {
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            warning: None,
            hint: None,
            footer_lines: Vec::new(),
            placeholder: None,
            required: true,
            trimmed: true,
            default: None,
            suggestions: &[],
            password: false,
            show_back: false,
        }
    }

    /// Show "esc go back" in key hints (for non-first wizard steps).
    pub fn show_back(mut self) -> Self {
        self.show_back = true;
        self
    }

    pub fn with_hint(mut self, hint: &'a str) -> Self {
        self.hint = Some(hint);
        self
    }

    pub fn with_footer_lines(mut self, footer_lines: Vec<String>) -> Self {
        self.footer_lines = footer_lines;
        self
    }

    pub fn with_warning(mut self, warning: &'a str) -> Self {
        self.warning = Some(warning);
        self
    }

    /// Dimmed text shown when input is empty. Falls back to first suggestion.
    pub fn with_placeholder(mut self, placeholder: &'a str) -> Self {
        self.placeholder = Some(placeholder);
        self
    }

    /// Allow empty input (Enter with no text). Fields are required by default.
    /// Sets hint to "optional" unless already overridden by `.with_hint()`.
    pub fn optional(mut self) -> Self {
        self.required = false;
        if self.hint.is_none() {
            self.hint = Some("optional");
        }
        self
    }

    pub fn with_default(mut self, default: &'a str) -> Self {
        self.default = Some(default);
        self
    }

    pub fn default_opt(mut self, default: Option<&'a str>) -> Self {
        self.default = default;
        self
    }

    pub fn suggestions(mut self, suggestions: &'a [String]) -> Self {
        self.suggestions = suggestions;
        self
    }

    // CodeQL[rust/hard-coded-cryptographic-value]: bool flag for input masking, not a credential
    pub fn password(mut self) -> Self {
        self.password = true;
        self.trimmed = false;
        self
    }

    pub fn prompt(self) -> io::Result<String> {
        self.prompt_validated(|_| Ok(()))
    }

    pub fn prompt_validated(
        self,
        mut validate: impl FnMut(&str) -> Result<(), String>,
    ) -> io::Result<String> {
        if !is_interactive() {
            if self.password {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Password prompt requires an interactive terminal",
                ));
            }
            let value = match self.default {
                Some(value) => Ok(value.to_string()),
                None => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "Missing required value: {}. In --ci mode, pass the value via a CLI flag or config.",
                        self.label
                    ),
                )),
            }?;
            validate(&value)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
            return Ok(value);
        }

        // Verbose mode: single-line prompt with › separator, no screen erasing.
        if !is_pretty() {
            let mut error: Option<String> = None;
            loop {
                if let Some(warning) = self.warning {
                    eprintln!("{} {}", bold(&theme_warning("!")), theme_warning(warning));
                }
                let active_error = error.as_deref();
                if let Some(message) = active_error {
                    eprintln!("{} {}", bold(&theme_error("✘")), theme_error(message));
                }
                let label = if active_error.is_some() {
                    theme_error(self.label)
                } else {
                    theme_accent(self.label)
                };
                let display_label = match self.hint {
                    Some(hint) => {
                        let hint = format!("({hint})");
                        let hint = if active_error.is_some() {
                            theme_error(hint)
                        } else {
                            theme_muted(hint)
                        };
                        format!("{label} {hint}")
                    }
                    None => label,
                };
                let value = raw_text_input(
                    &display_label,
                    RawTextInputOptions {
                        initial: if active_error.is_some() {
                            None
                        } else {
                            self.default
                        },
                        suggestions: self.suggestions,
                        password: self.password,
                        placeholder_override: self.placeholder,
                        required: self.required,
                        trimmed: self.trimmed,
                        use_separator: true, // use › separator
                        error: active_error.is_some(),
                    },
                )?;
                match validate(&value) {
                    Ok(()) => return Ok(value),
                    Err(message) => error = Some(message),
                }
            }
        }

        // Pretty mode: multi-line diamond design
        let term = console::Term::stderr();
        let hint_lines = self
            .hint
            .map(|hint| vec![super::format_pretty_prompt_hint_line(hint)]);
        let footer_spacing = usize::from(!self.footer_lines.is_empty());
        // Lines below the input: content hints (optional) + key hints
        let below_input_count = hint_lines.as_ref().map_or(0, |l| l.len())
            + 1
            + footer_spacing
            + self.footer_lines.len();

        let mut error: Option<String> = None;
        loop {
            let active_error = error.as_deref();
            for line in super::format_pretty_prompt_header(self.label, self.warning, active_error) {
                eprintln!("{line}");
            }

            // Reserve space: blank for input, then hint lines, then key hints
            eprintln!();
            if let Some(lines) = &hint_lines {
                for line in lines {
                    eprintln!("{line}");
                }
            }
            eprintln!("{}", super::format_key_hints(self.show_back));
            if !self.footer_lines.is_empty() {
                eprintln!();
                for line in &self.footer_lines {
                    eprintln!("{line}");
                }
            }
            let _ = crossterm::execute!(
                io::stderr(),
                crossterm::cursor::MoveUp((1 + below_input_count) as u16),
                crossterm::cursor::MoveToColumn(0)
            );

            let value = match raw_text_input(
                &super::format_pretty_prompt_input_prefix_for_status(true, active_error.is_some()),
                RawTextInputOptions {
                    initial: if active_error.is_some() {
                        None
                    } else {
                        self.default
                    },
                    suggestions: self.suggestions,
                    password: self.password,
                    placeholder_override: self.placeholder,
                    required: self.required,
                    trimmed: self.trimmed,
                    use_separator: false, // no › separator in pretty mode
                    error: active_error.is_some(),
                },
            ) {
                Ok(v) => v,
                Err(e) => {
                    let _ = crossterm::execute!(
                        io::stderr(),
                        crossterm::cursor::MoveDown(below_input_count as u16),
                        crossterm::cursor::MoveToColumn(0)
                    );
                    let num_rows = pretty_text_prompt_active_lines(
                        self.warning,
                        active_error,
                        self.hint,
                        footer_spacing,
                        self.footer_lines.len(),
                    );
                    let _ = term.clear_last_lines(num_rows);
                    if e.kind() == io::ErrorKind::Interrupted && !super::is_wizard_back(&e) {
                        for line in super::format_pretty_cancelled_prompt(self.label) {
                            let _ = term.write_line(&line);
                        }
                    }
                    return Err(e);
                }
            };

            let validation = validate(&value);
            let _ = crossterm::execute!(
                io::stderr(),
                crossterm::cursor::MoveDown(below_input_count as u16),
                crossterm::cursor::MoveToColumn(0)
            );

            let num_rows = pretty_text_prompt_active_lines(
                self.warning,
                active_error,
                self.hint,
                footer_spacing,
                self.footer_lines.len(),
            );
            let _ = term.clear_last_lines(num_rows);

            match validation {
                Ok(()) => {
                    let done_value = if self.password && value.is_empty() && !self.required {
                        String::new()
                    } else if self.password {
                        theme_muted("••••••").to_string()
                    } else {
                        value.clone()
                    };
                    for line in super::format_pretty_text_prompt_completion(
                        self.label,
                        self.warning,
                        &done_value,
                    ) {
                        let _ = term.write_line(&line);
                    }

                    return Ok(value);
                }
                Err(message) => {
                    error = Some(message);
                }
            }
        }
    }
}

/// Custom text input using crossterm. Supports cursor movement, word deletion,
/// tab-completion from suggestions, inline auto-suggest, password masking, and placeholder text.
///
/// `use_separator`: if true, renders `{prompt} › {cursor}` (verbose style).
/// If false, renders `{prompt}{cursor}` — the caller has already printed the label
/// and `prompt` is any optional indentation prefix.
struct RawTextInputOptions<'a> {
    initial: Option<&'a str>,
    suggestions: &'a [String],
    password: bool,
    placeholder_override: Option<&'a str>,
    required: bool,
    trimmed: bool,
    use_separator: bool,
    error: bool,
}

fn raw_text_input(prompt: &str, options: RawTextInputOptions<'_>) -> io::Result<String> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
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
        use_separator,
        error,
    } = options;

    let mut buf: Vec<char> = initial.unwrap_or("").chars().collect();
    let mut pos: usize = buf.len(); // cursor position in chars
    let mut suggestion_idx: Option<usize> = None;

    // Placeholder: explicit override > first suggestion > dots for password
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

    // Find the best starts-with match for inline auto-suggest (fish-shell style).
    let sep_display_width: usize = if use_separator { 3 } else { 0 }; // " › " = 3

    let inline_suffix = |buf: &[char]| -> String {
        if buf.is_empty() || suggestions.is_empty() || password {
            return String::new();
        }
        let current: String = buf.iter().collect();
        let lower = current.to_lowercase();
        for s in suggestions {
            if s.to_lowercase().starts_with(&lower) && s.len() > current.len() {
                // Use char-based slicing for multi-byte safety
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
            let display: String = if password {
                "•".repeat(buf.len())
            } else {
                buf.iter().collect()
            };
            let display = if error { theme_error(display) } else { display };
            if use_separator {
                let _ = write!(out, "{prompt} {separator} {display}");
            } else {
                let _ = write!(out, "{prompt}{display}");
            }
            // Show inline suggestion suffix dimmed (only when cursor is at end)
            if !suffix.is_empty() && pos == buf.len() {
                let suffix = if error {
                    theme_error(suffix)
                } else {
                    theme_dim(suffix)
                };
                let _ = write!(out, "{suffix}");
            }
        }
        // Position cursor: prompt_width + sep_width + chars-before-cursor
        let prompt_width = console::measure_text_width(prompt);
        let cursor_offset = if password {
            pos
        } else {
            buf[..pos].iter().collect::<String>().len()
        };
        let col = prompt_width + sep_display_width + cursor_offset;
        let _ = crossterm::execute!(*out, cursor::MoveToColumn(col as u16));
        let _ = out.flush();
    };

    // Accept the current inline suggestion into the buffer.
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

    // Draw initial state
    terminal::enable_raw_mode()?;
    let _ = crossterm::execute!(out, cursor::Show);

    // Set cursor color to brand teal
    let (cr, cg, cb) = ACCENT;
    let _ = write!(out, "\x1b]12;rgb:{cr:02x}/{cg:02x}/{cb:02x}\x1b\\");
    let _ = out.flush();

    let suf = inline_suffix(&buf);
    draw(&buf, pos, &mut out, password, &placeholder, &suf);

    let result = loop {
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
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
                    break Err(wizard_back_error());
                }
                // Character input
                KeyCode::Char(c)
                    if !modifiers.contains(KeyModifiers::CONTROL)
                        && !modifiers.contains(KeyModifiers::ALT) =>
                {
                    // Reject leading whitespace when trimmed
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
                // Backspace
                KeyCode::Backspace
                    if modifiers.contains(KeyModifiers::SUPER)
                        || modifiers.contains(KeyModifiers::ALT) =>
                {
                    // Word/line delete backward
                    if pos > 0 {
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
                }
                KeyCode::Backspace => {
                    if pos > 0 {
                        pos -= 1;
                        buf.remove(pos);
                        suggestion_idx = None;
                    }
                }
                KeyCode::Delete => {
                    if pos < buf.len() {
                        buf.remove(pos);
                    }
                }
                // Cursor movement
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
                        // At end of buffer: accept inline suggestion
                        accept_inline(&mut buf, &mut pos, suggestions);
                        suggestion_idx = None;
                    }
                }
                KeyCode::Home | KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
                    pos = 0;
                }
                KeyCode::End | KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
                    pos = buf.len();
                }
                // Kill to end of line
                KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.truncate(pos);
                }
                // Kill to start of line
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.drain(..pos);
                    pos = 0;
                }
                // Tab completion: cycle through matching suggestions
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
    };

    // Hide cursor again if it's globally hidden (we only showed it for
    // the duration of active text input).
    if crate::output::cursor::is_cursor_globally_hidden() {
        let _ = crossterm::execute!(out, crossterm::cursor::Hide);
    }
    terminal::disable_raw_mode()?;

    // Restore default cursor color
    let _ = write!(out, "\x1b]112\x1b\\");

    // Move to next line
    let _ = write!(out, "\r\n");
    let _ = out.flush();

    result
}

fn pretty_text_prompt_active_lines(
    warning: Option<&str>,
    error: Option<&str>,
    hint: Option<&str>,
    footer_spacing: usize,
    footer_lines: usize,
) -> usize {
    // label + input + key hints + optional warning/error + optional hint + footer block
    3 + warning.is_some() as usize
        + error.is_some() as usize
        + hint.is_some() as usize
        + footer_spacing
        + footer_lines
}

/// Used internally by filter_suggestions tests — kept for test compatibility.
#[cfg(test)]
fn filter_suggestions(suggestions: &[String], current_input: &str) -> Vec<String> {
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

#[cfg(test)]
mod tests {
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
}
