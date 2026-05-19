use std::io;

use super::super::super::theme_accent;

#[derive(Clone, Copy)]
pub(super) struct PromptValidationStatus {
    pub(super) marker_offset: u16,
}

pub(super) fn prompt_validation_marker_offset(warning: Option<&str>) -> u16 {
    2 + warning.is_some() as u16
}

pub(super) fn run_validation_with_prompt_spinner<F>(
    value: String,
    status: PromptValidationStatus,
    validate: std::sync::Arc<F>,
) -> Result<(), String>
where
    F: Fn(String) -> Result<(), String> + Send + Sync + 'static,
{
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = validate(value);
        let _ = tx.send(result);
    });

    match rx.recv_timeout(Duration::from_secs(1)) {
        Ok(result) => return result,
        Err(RecvTimeoutError::Disconnected) => {
            return Err("Validation failed. Try again.".to_string());
        }
        Err(RecvTimeoutError::Timeout) => {}
    }

    let _guard = PromptValidationCursorGuard;
    let _ = crossterm::execute!(io::stderr(), crossterm::cursor::Hide);
    let mut tick = 0usize;
    loop {
        draw_prompt_validation_marker(status, tick);
        tick = tick.wrapping_add(1);
        match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Validation failed. Try again.".to_string());
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

struct PromptValidationCursorGuard;

impl Drop for PromptValidationCursorGuard {
    fn drop(&mut self) {
        if !crate::output::cursor::is_cursor_globally_hidden() {
            let _ = crossterm::execute!(io::stderr(), crossterm::cursor::Show);
        }
    }
}

fn draw_prompt_validation_marker(status: PromptValidationStatus, tick: usize) {
    use std::io::Write;

    let spinner = crate::output::SPINNER_TICKS[tick % crate::output::SPINNER_TICKS.len()];
    let marker = theme_accent(spinner);
    let mut out = io::stderr();
    let _ = crossterm::execute!(
        out,
        crossterm::cursor::SavePosition,
        crossterm::cursor::MoveUp(status.marker_offset),
        crossterm::cursor::MoveToColumn(0)
    );
    let _ = write!(out, "{marker}");
    let _ = crossterm::execute!(out, crossterm::cursor::RestorePosition);
    let _ = out.flush();
}
