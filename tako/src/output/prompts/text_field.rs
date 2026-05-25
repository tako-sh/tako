mod raw;
mod validation;

use std::io;

use super::super::{
    is_interactive, is_pretty, theme_accent, theme_error, theme_muted, theme_warning,
};
use raw::{RawTextInputOptions, raw_text_input};
use validation::{
    PromptValidationStatus, prompt_validation_marker_offset, run_validation_with_prompt_spinner,
};

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

    pub(super) fn completion_display_value(&self, value: &str) -> String {
        if self.password && value.is_empty() && !self.required {
            String::new()
        } else if self.password {
            theme_muted("••••••").to_string()
        } else {
            value.to_string()
        }
    }

    pub fn prompt(self) -> io::Result<String> {
        self.prompt_validated(|_| Ok(()))
    }

    pub fn prompt_validated(
        self,
        mut validate: impl FnMut(&str) -> Result<(), String>,
    ) -> io::Result<String> {
        self.prompt_validated_inner(|value, _status| validate(value))
    }

    pub fn prompt_validated_with_spinner<F>(self, validate: F) -> io::Result<String>
    where
        F: Fn(String) -> Result<(), String> + Send + Sync + 'static,
    {
        let validate = std::sync::Arc::new(validate);
        self.prompt_validated_inner(move |value, status| {
            if let Some(status) = status {
                run_validation_with_prompt_spinner(value.to_string(), status, validate.clone())
            } else {
                validate(value.to_string())
            }
        })
    }

    fn prompt_validated_inner(
        self,
        mut validate: impl FnMut(&str, Option<PromptValidationStatus>) -> Result<(), String>,
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
            validate(&value, None)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
            return Ok(value);
        }

        if !is_pretty() {
            return self.prompt_verbose(validate);
        }

        self.prompt_pretty(validate)
    }

    fn prompt_verbose(
        self,
        mut validate: impl FnMut(&str, Option<PromptValidationStatus>) -> Result<(), String>,
    ) -> io::Result<String> {
        let mut error: Option<String> = None;
        loop {
            if let Some(warning) = self.warning {
                // CodeQL[rust/cleartext-logging]: prompt warnings are UI copy; password input is masked below.
                eprintln!("{}", theme_warning(warning));
            }
            let active_error = error.as_deref();
            if let Some(message) = active_error {
                eprintln!("{}", theme_error(message));
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
                    use_separator: true,
                    error: active_error.is_some(),
                    show_back: self.show_back,
                },
            )?;
            match validate(&value, None) {
                Ok(()) => return Ok(value),
                Err(message) => error = Some(message),
            }
        }
    }

    fn prompt_pretty(
        self,
        mut validate: impl FnMut(&str, Option<PromptValidationStatus>) -> Result<(), String>,
    ) -> io::Result<String> {
        let term = console::Term::stderr();
        let hint_lines = self
            .hint
            .map(|hint| vec![super::format_pretty_prompt_hint_line(hint)]);
        let footer_spacing = usize::from(!self.footer_lines.is_empty());

        let mut error: Option<String> = None;
        loop {
            let active_error = error.as_deref();
            let below_input_count = active_error.is_some() as usize
                + hint_lines.as_ref().map_or(0, |l| l.len())
                + 1
                + footer_spacing
                + self.footer_lines.len();
            for line in super::format_pretty_prompt_header(self.label, self.warning, active_error) {
                eprintln!("{line}");
            }

            eprintln!();
            if let Some(message) = active_error {
                eprintln!("{}", super::format_pretty_prompt_error_line(message));
            }
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
                    use_separator: false,
                    error: active_error.is_some(),
                    show_back: self.show_back,
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

            let validation = validate(
                &value,
                Some(PromptValidationStatus {
                    marker_offset: prompt_validation_marker_offset(self.warning),
                }),
            );
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
                    let done_value = self.completion_display_value(&value);
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

#[cfg(test)]
mod tests;
