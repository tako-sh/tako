use std::io;

use console::Term;

use super::super::{DIAMOND_OUTLINED, is_interactive, is_pretty, theme_muted};
use super::select::raw_select;
use super::text_field::TextField;
use super::{
    confirm_with_description, format_pretty_confirm_completion, format_pretty_prompt_completion,
    format_pretty_text_prompt_completion, is_wizard_back, wizard_back_error,
};

struct WizardField {
    label: String,
    value: Option<String>,
    visible: bool,
    /// Whether the field has been shown as an active prompt at least once.
    visited: bool,
    /// Pre-formatted completion lines for this field (includes trailing blank spacer).
    /// Empty when the field hasn't been answered in this wizard session.
    completion_lines: Vec<String>,
}

pub struct Wizard {
    fields: Vec<WizardField>,
    confirmation: bool,
    /// Total lines currently on screen from wizard output (block + prompt completions).
    /// Used by `render()` to atomically replace the display via cursor-up + clear-to-end.
    rendered_lines: usize,
    /// Label of the field currently being prompted (excluded from format_block).
    active_label: Option<String>,
}

impl Wizard {
    pub fn new() -> Self {
        Self {
            fields: Vec::new(),
            confirmation: false,
            rendered_lines: 0,
            active_label: None,
        }
    }

    /// Define all fields upfront with their order and subsection grouping.
    /// Each entry is `(label, subsection)`. Subsection fields start hidden.
    pub fn with_fields(mut self, fields: &[(&str, bool)]) -> Self {
        self.fields = fields
            .iter()
            .map(|(label, subsection)| WizardField {
                label: label.to_string(),
                value: None,
                visible: !subsection,
                visited: false,
                completion_lines: Vec::new(),
            })
            .collect();
        self
    }

    /// Enable a "Looks good?" confirmation prompt at the end of the wizard.
    pub fn with_confirmation(mut self) -> Self {
        self.confirmation = true;
        self
    }

    /// Pre-populate a field's value (for defaults). Does not generate completion
    /// lines — the field won't appear in the rendered block until actively answered.
    pub fn set(&mut self, label: &str, value: &str) {
        if let Some(field) = self.fields.iter_mut().find(|f| f.label == label) {
            field.value = if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            };
        }
    }

    /// Store a field's answered value and its formatted completion lines.
    fn set_completed(&mut self, label: &str, value: &str, lines: Vec<String>) {
        self.active_label = None;
        if let Some(field) = self.fields.iter_mut().find(|f| f.label == label) {
            field.value = if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            };
            field.completion_lines = lines;
        }
    }

    /// Set visibility for a field.
    pub fn set_visible(&mut self, label: &str, visible: bool) {
        if let Some(field) = self.fields.iter_mut().find(|f| f.label == label) {
            field.visible = visible;
            if !visible {
                field.value = None;
                field.visited = false;
                field.completion_lines.clear();
            }
        }
    }

    /// Remove the last visible answered field's value and completion
    /// (for correcting invalid input like bad port numbers).
    pub fn undo_last(&mut self) {
        if let Some(field) = self
            .fields
            .iter_mut()
            .rev()
            .find(|f| f.visible && f.value.is_some())
        {
            field.value = None;
            field.completion_lines.clear();
        }
    }

    /// Whether this is the first actively completed field (no previous fields
    /// with completion lines). Pre-populated fields (value only, no completion
    /// lines) don't count — they haven't been confirmed in this session.
    fn is_first_field(&self, label: &str) -> bool {
        let idx = self
            .fields
            .iter()
            .position(|f| f.label == label)
            .unwrap_or(0);
        !self.fields[..idx]
            .iter()
            .any(|f| f.visible && !f.completion_lines.is_empty())
    }

    /// Mark a field as the active prompt. Prior completed fields remain above
    /// the prompt and later visited fields can be rendered below it.
    fn prepare_prompt(&mut self, label: &str) -> bool {
        let first = self.is_first_field(label);
        if let Some(field) = self.fields.iter_mut().find(|f| f.label == label) {
            field.visited = true;
        }
        self.active_label = Some(label.to_string());
        first
    }

    /// Clear all field values and completion lines (for "Looks good?" → No restart).
    pub fn reset(&mut self) {
        for field in &mut self.fields {
            field.value = None;
            field.visited = false;
            field.completion_lines.clear();
        }
    }

    /// Concatenate all completed/visited fields' formatted lines into a single block.
    /// The active field is rendered separately by the prompt itself, so the block
    /// only shows inactive fields: completed summaries plus visited placeholders.
    #[cfg(test)]
    fn format_block(&self) -> Vec<String> {
        let (before, after) = self.format_blocks();
        before.into_iter().chain(after).collect()
    }

    fn format_inactive_field(field: &WizardField) -> Vec<String> {
        if !field.completion_lines.is_empty() {
            field.completion_lines.clone()
        } else if field.visited {
            let diamond = theme_muted(DIAMOND_OUTLINED);
            let label = theme_muted(&field.label);
            vec![
                format!("{diamond} {label}"),
                theme_muted("›").to_string(),
                String::new(),
            ]
        } else {
            Vec::new()
        }
    }

    /// Split inactive wizard lines into fields before the active prompt and fields
    /// after it. The suffix block is rendered below the active prompt so back
    /// navigation keeps later visited steps visible in their original order.
    fn format_blocks(&self) -> (Vec<String>, Vec<String>) {
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut past_active = false;
        for field in &self.fields {
            if !field.visible {
                continue;
            }
            if self.active_label.as_deref() == Some(&field.label) {
                past_active = true;
                continue;
            }
            let target = if past_active { &mut after } else { &mut before };
            let formatted = Self::format_inactive_field(field);
            if !formatted.is_empty() {
                target.extend(formatted);
            }
        }
        (before, after)
    }

    fn trailing_block(&self) -> Vec<String> {
        self.format_blocks().1
    }

    /// Replace the wizard's on-screen output with the current completed-fields
    /// block. Uses Term::clear_last_lines + Term::write_line for consistency
    /// with the prompt functions. No-op in verbose or non-interactive mode.
    fn render(&mut self) {
        if !is_pretty() || !is_interactive() {
            return;
        }
        let lines = self.format_blocks().0;
        let term = Term::stderr();
        if self.rendered_lines > 0 {
            let _ = term.clear_last_lines(self.rendered_lines);
        }
        for line in &lines {
            let _ = term.write_line(line);
        }
        self.rendered_lines = lines.len();
    }

    pub fn input(
        &mut self,
        label: &str,
        default: Option<&str>,
        info: Option<&str>,
    ) -> io::Result<String> {
        let first = self.prepare_prompt(label);
        let footer_lines = self.trailing_block();
        loop {
            self.render();
            let mut builder = TextField::new(label).default_opt(default);
            if let Some(text) = info {
                builder = builder.with_warning(text);
            }
            if !footer_lines.is_empty() {
                builder = builder.with_footer_lines(footer_lines.clone());
            }
            if !first {
                builder = builder.show_back();
            }
            match builder.prompt() {
                Ok(value) => {
                    let completion = format_pretty_text_prompt_completion(label, info, &value);
                    self.rendered_lines += completion.len();
                    self.set_completed(label, &value, completion);
                    return Ok(value);
                }
                Err(e) if is_wizard_back(&e) => {
                    if first {
                        continue;
                    }
                    return Err(wizard_back_error());
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn select<T: Clone>(
        &mut self,
        label: &str,
        prompt: &str,
        options: Vec<(String, T)>,
        hints: &[&str],
        default: usize,
    ) -> io::Result<T> {
        if options.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "No options available for selection",
            ));
        }

        let first = self.prepare_prompt(label);
        let footer_lines = self.trailing_block();
        loop {
            self.render();
            let labels: Vec<&str> = options.iter().map(|(l, _)| l.as_str()).collect();
            let select_result = raw_select(
                &Term::stderr(),
                prompt,
                &labels,
                hints,
                !first,
                default,
                &footer_lines,
            );
            match select_result {
                Ok(index) => {
                    let display_label = options[index].0.clone();
                    let value = options.into_iter().nth(index).unwrap().1;
                    let title = prompt.lines().next().unwrap_or(prompt);
                    // raw_select already wrote the completion to terminal
                    let completion = format_pretty_prompt_completion(title, &display_label);
                    self.rendered_lines += completion.len();
                    self.set_completed(label, &display_label, completion);
                    return Ok(value);
                }
                Err(e) if is_wizard_back(&e) => {
                    if first {
                        continue;
                    }
                    return Err(wizard_back_error());
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Accept a fully configured [`TextField`] builder and track the answer.
    pub fn text_field(&mut self, builder: TextField) -> io::Result<String> {
        let label = builder.label.to_string();
        self.text_field_named(&label, builder)
    }

    /// Accept a fully configured [`TextField`] builder and track it under a
    /// different wizard field label. This keeps navigation tied to a stable
    /// field while allowing the visible prompt text to be dynamic.
    pub fn text_field_named(
        &mut self,
        field_label: &str,
        builder: TextField,
    ) -> io::Result<String> {
        let prompt_label = builder.label.to_string();
        let warning = builder.warning.map(str::to_string);
        let first = self.prepare_prompt(field_label);
        let footer_lines = self.trailing_block();
        loop {
            self.render();
            let mut b = builder.clone();
            if !footer_lines.is_empty() {
                b = b.with_footer_lines(footer_lines.clone());
            }
            if !first {
                b = b.show_back();
            }
            match b.prompt() {
                Ok(value) => {
                    let completion = format_pretty_text_prompt_completion(
                        &prompt_label,
                        warning.as_deref(),
                        &value,
                    );
                    self.rendered_lines += completion.len();
                    self.set_completed(field_label, &value, completion);
                    return Ok(value);
                }
                Err(e) if is_wizard_back(&e) => {
                    if first {
                        continue;
                    }
                    return Err(wizard_back_error());
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn confirm(&mut self, prompt: &str) -> io::Result<bool> {
        self.confirm_default(prompt, prompt, true)
    }

    pub fn confirm_default(
        &mut self,
        field_label: &str,
        prompt: &str,
        default: bool,
    ) -> io::Result<bool> {
        let first = self.prepare_prompt(field_label);
        loop {
            self.render();
            match confirm_with_description(prompt, None, default) {
                Ok(answer) => {
                    let answer_text = if answer { "yes" } else { "no" };
                    let completion = format_pretty_confirm_completion(prompt, default, answer_text);
                    self.rendered_lines += completion.len();
                    self.set_completed(field_label, answer_text, completion);
                    return Ok(answer);
                }
                Err(e) if is_wizard_back(&e) => {
                    if first {
                        continue;
                    }
                    return Err(wizard_back_error());
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Finalize the wizard. If confirmation is enabled, shows a "Looks good?" prompt.
    /// Returns `Ok(true)` to proceed, `Ok(false)` to restart from step 0.
    /// ESC goes back one step via `wizard_back`.
    pub fn finish(&mut self) -> io::Result<bool> {
        if !self.confirmation {
            return Ok(true);
        }
        match self.confirm("Looks good?") {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.reset();
                Ok(false)
            }
            Err(e) if is_wizard_back(&e) => {
                // Confirm cleared itself. The previous step's clear_field will
                // handle removing its field from the block when re-prompted.
                Err(wizard_back_error())
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wizard_format_block_concatenates_completed_fields() {
        let mut w = Wizard::new().with_fields(&[("Name", false), ("Runtime", false)]);
        w.set_completed(
            "Name",
            "tako",
            vec!["◇ Name".into(), "› tako".into(), String::new()],
        );
        w.set_completed(
            "Runtime",
            "bun",
            vec!["◇ Runtime".into(), "› bun".into(), String::new()],
        );
        assert_eq!(
            w.format_block(),
            vec![
                "◇ Name".to_string(),
                "› tako".to_string(),
                String::new(),
                "◇ Runtime".to_string(),
                "› bun".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn wizard_format_block_skips_hidden_fields() {
        let mut w = Wizard::new().with_fields(&[("A", false), ("B", true)]);
        w.set_completed("A", "x", vec!["A".into()]);
        w.set_completed("B", "y", vec!["B".into()]);
        // B is subsection — starts hidden
        assert_eq!(w.format_block(), vec!["A".to_string()]);
    }

    #[test]
    fn wizard_reset_clears_all_fields() {
        let mut w = Wizard::new().with_fields(&[("A", false), ("B", false)]);
        w.set_completed("A", "x", vec!["a".into()]);
        w.set_completed("B", "y", vec!["b".into()]);
        w.reset();
        assert!(w.fields.iter().all(|f| f.value.is_none()));
        assert!(w.fields.iter().all(|f| f.completion_lines.is_empty()));
        assert!(w.format_block().is_empty());
    }

    #[test]
    fn wizard_is_first_field_ignores_prepopulated() {
        let mut w = Wizard::new().with_fields(&[("A", false), ("B", false)]);
        // Pre-populate A with set() — no completion lines
        w.set("A", "default");
        assert!(
            w.is_first_field("B"),
            "B should be first since A has no completion"
        );
        // Now actively complete A
        w.set_completed("A", "val", vec!["done".into()]);
        assert!(!w.is_first_field("B"), "B should no longer be first");
    }

    #[test]
    fn wizard_set_visible_false_clears_completion() {
        let mut w = Wizard::new().with_fields(&[("A", false)]);
        w.set_completed("A", "x", vec!["a".into()]);
        w.set_visible("A", false);
        assert!(w.fields[0].completion_lines.is_empty());
        assert!(w.fields[0].value.is_none());
    }

    #[test]
    fn wizard_prepare_prompt_hides_active_completion() {
        let mut w = Wizard::new().with_fields(&[("A", false), ("B", false)]);
        w.set_completed("A", "x", vec!["a".into()]);
        w.set_completed("B", "y", vec!["b".into()]);

        let first = w.prepare_prompt("B");

        assert!(!first);
        let block = w.format_block();
        assert_eq!(block, vec!["a".to_string()]);
        assert_eq!(w.fields[1].value.as_deref(), Some("y"));
        assert!(!w.fields[1].completion_lines.is_empty());
        assert!(w.fields[1].visited);
        assert_eq!(w.active_label.as_deref(), Some("B"));
    }

    #[test]
    fn wizard_prepare_prompt_keeps_later_visited_placeholders_visible() {
        let mut w = Wizard::new().with_fields(&[("A", false), ("B", false)]);
        w.set_completed("A", "x", vec!["a".into()]);

        let _ = w.prepare_prompt("B");
        let first = w.prepare_prompt("A");

        assert!(first);
        let block = w.format_block();
        assert_eq!(
            block,
            vec!["◇ B".to_string(), "›".to_string(), String::new(),]
        );
        assert!(w.fields[1].visited);
        assert!(w.fields[1].completion_lines.is_empty());
        assert_eq!(w.active_label.as_deref(), Some("A"));
    }

    #[test]
    fn wizard_undo_last_clears_completion_lines() {
        let mut w = Wizard::new().with_fields(&[("A", false), ("B", false)]);
        w.set_completed("A", "x", vec!["a".into()]);
        w.set_completed("B", "y", vec!["b".into()]);
        w.undo_last();
        // B should be undone
        assert!(w.fields[1].value.is_none());
        assert!(w.fields[1].completion_lines.is_empty());
        // A untouched
        assert_eq!(w.fields[0].value.as_deref(), Some("x"));
    }
}
