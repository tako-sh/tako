mod phase;
mod transfer;

pub use phase::PhaseSpinner;
pub use transfer::{TrackedSpinner, TransferProgress, format_transfer_compact_detail};

use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

use super::cursor::{
    clear_active_progress_bar, hide_cursor, register_active_progress_bar, show_cursor,
};
use super::{
    ACCENT, PHASE_PB, bold, emit, error_block, format_elapsed_suffix, format_success_elapsed_line,
    is_interactive, is_pretty, should_colorize, theme_error, theme_fg,
};

// ---------------------------------------------------------------------------
// Spinner helpers
// ---------------------------------------------------------------------------

pub const SPINNER_TICKS: &[&str] = &["✶", "✸", "✹", "✺", "✹", "✷"];

fn teal_spinner_token() -> String {
    if should_colorize() {
        let (r, g, b) = ACCENT;
        format!("\x1b[38;2;{r};{g};{b}m{{spinner}}\x1b[39m")
    } else {
        "{spinner}".to_string()
    }
}

pub fn spinner_style() -> ProgressStyle {
    let s = teal_spinner_token();
    ProgressStyle::with_template(&format!("{s} {{msg}}"))
        .unwrap()
        .tick_strings(SPINNER_TICKS)
}

pub(super) fn phase_spinner_style() -> ProgressStyle {
    let s = teal_spinner_token();
    ProgressStyle::with_template(&format!("{s} {{msg}}"))
        .unwrap()
        .tick_strings(SPINNER_TICKS)
}

/// Print a spinner result without elapsed (fast path — spinner was never shown).
///
/// Only emits in pretty mode. In verbose/CI mode spinners are silent — the
/// caller's `output::timed()` owns action tracing.
fn print_ok(success_msg: &str, elapsed: Duration) {
    if !is_pretty() {
        return;
    }
    emit(&format_success_elapsed_line(success_msg, elapsed));
}

/// Emit `✘ {label}` as a standalone failure indicator line, optionally with elapsed time.
fn format_error_label_line(label: &str, elapsed: Option<Duration>) -> String {
    let check = bold(&theme_error("✘"));
    let time_str = elapsed
        .map(format_elapsed_suffix)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    format!("{check} {}{time_str}", theme_fg(label))
}

/// Emit `✘ {label}` as a standalone failure indicator line, optionally with elapsed time.
fn emit_error_label(label: &str, elapsed: Option<Duration>) {
    emit(&format_error_label_line(label, elapsed));
}

fn print_err(loading: &str) {
    if is_pretty() {
        emit_error_label(loading, None);
    } else {
        // Errors stay visible in verbose/CI mode even though success is silent.
        tracing::error!("{}", loading);
    }
}

fn print_err_with_detail(loading: &str, detail: &dyn Display, elapsed: Option<Duration>) {
    if is_pretty() {
        emit_error_label(loading, elapsed);
        error_block(&detail.to_string());
    } else {
        tracing::error!("{}: {}", loading, detail);
    }
}

pub(super) fn finish_spinner_ok(pb: &ProgressBar, success_msg: &str, elapsed: Duration) {
    clear_active_progress_bar();
    pb.finish_and_clear();
    show_cursor();
    emit(&format_success_elapsed_line(success_msg, elapsed));
}

fn finish_spinner_err(pb: &ProgressBar, loading: &str, elapsed: Duration) {
    clear_active_progress_bar();
    pb.finish_and_clear();
    show_cursor();
    emit_error_label(loading, Some(elapsed));
}

fn finish_spinner_err_with_detail(
    pb: &ProgressBar,
    loading: &str,
    detail: &dyn Display,
    elapsed: Duration,
) {
    clear_active_progress_bar();
    pb.finish_and_clear();
    show_cursor();
    emit_error_label(loading, Some(elapsed));
    error_block(&detail.to_string());
}

/// Run work silently — no spinner, no success output. Errors still print.
/// Used for preflight checks where only failures matter.
pub fn with_spinner_silent<T, E: Display, F>(loading: &str, work: F) -> Result<T, E>
where
    F: FnOnce() -> Result<T, E>,
{
    let result = work();
    if let Err(ref e) = result {
        if is_pretty() {
            emit_error_label(loading, None);
            error_block(&e.to_string());
        } else {
            tracing::error!("{}: {}", loading, e);
        }
    }
    result
}

/// Spinner that shows only if work takes >= 1s, then clears on completion.
///
/// - Fast (<1s):  prints result directly, no spinner, no elapsed
/// - Slow (≥1s):  `⠋ {loading}…` → `✔ {success} elapsed` or `✘ {loading} elapsed`
///
/// In verbose/CI mode: silent on success — the caller's `output::timed()`
/// owns action tracing. Errors still emit.
pub fn with_spinner<T, E: Display, F>(loading: &str, success: &str, work: F) -> Result<T, E>
where
    F: FnOnce() -> Result<T, E>,
{
    if !is_pretty() {
        let result = work();
        if let Err(ref e) = result {
            tracing::error!("{}: {}", loading, e);
        }
        return result;
    }

    if !is_interactive() {
        return work();
    }

    // When a PhaseSpinner is active, run work silently — the phase spinner
    // already shows progress. Errors still emit so failures are visible.
    if PHASE_PB.lock().unwrap().is_some() {
        let result = work();
        if result.is_err() {
            emit_error_label(loading, None);
        }
        return result;
    }

    let start = Instant::now();
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());

    // Enable spinner after 1s if work is still running.
    let pb_clone = pb.clone();
    let loading_str = loading.to_string();
    let spinner_shown = Arc::new(AtomicBool::new(false));
    let shown_clone = spinner_shown.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        if !pb_clone.is_finished() {
            shown_clone.store(true, Ordering::Relaxed);
            hide_cursor();
            register_active_progress_bar(&pb_clone);
            pb_clone.set_message(format!("{loading_str}…"));
            pb_clone.enable_steady_tick(Duration::from_millis(80));
        }
    });

    let result = work();
    let elapsed = start.elapsed();

    if spinner_shown.load(Ordering::Relaxed) {
        match &result {
            Ok(_) => finish_spinner_ok(&pb, success, elapsed),
            Err(_) => finish_spinner_err(&pb, loading, elapsed),
        }
    } else {
        clear_active_progress_bar();
        pb.finish_and_clear();
        match &result {
            Ok(_) => print_ok(success, elapsed),
            Err(_) => print_err(loading),
        }
    }

    result
}

/// Async spinner that shows only if work takes >= 1s, then clears on completion.
pub async fn with_spinner_async<T, E: Display, Fut>(
    loading: &str,
    success: &str,
    work: Fut,
) -> Result<T, E>
where
    Fut: Future<Output = Result<T, E>>,
{
    with_spinner_async_err(loading, success, loading, work).await
}

pub async fn with_spinner_async_err<T, E: Display, Fut>(
    loading: &str,
    success: &str,
    error_label: &str,
    work: Fut,
) -> Result<T, E>
where
    Fut: Future<Output = Result<T, E>>,
{
    if !is_pretty() {
        let result = work.await;
        if let Err(ref e) = result {
            tracing::error!("{}: {}", error_label, e);
        }
        return result;
    }

    if !is_interactive() {
        return work.await;
    }

    // When a PhaseSpinner is active, run work silently — the phase spinner
    // already shows progress. Errors still emit so failures are visible.
    if PHASE_PB.lock().unwrap().is_some() {
        let result = work.await;
        if let Err(e) = &result {
            emit_error_label(error_label, None);
            error_block(&e.to_string());
        }
        return result;
    }

    let start = Instant::now();
    let mut work = std::pin::pin!(work);

    // Fast path: complete within 1s — no spinner needed.
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), work.as_mut()).await {
        let elapsed = start.elapsed();
        match &result {
            Ok(_) => print_ok(success, elapsed),
            Err(e) => print_err_with_detail(error_label, e, Some(elapsed)),
        }
        return result;
    }

    // Slow path: show spinner for the remainder.
    hide_cursor();
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());
    pb.set_message(format!("{loading}…"));
    pb.enable_steady_tick(Duration::from_millis(80));
    register_active_progress_bar(&pb);

    let result = work.await;
    let elapsed = start.elapsed();

    match &result {
        Ok(_) => finish_spinner_ok(&pb, success, elapsed),
        Err(e) => finish_spinner_err_with_detail(&pb, error_label, e, elapsed),
    }

    result
}

/// Async simple spinner — shows only if work takes >= 1s, then clears. No result line.
/// In verbose/CI mode: prints a tracing line for the action.
pub async fn with_spinner_async_simple<T, Fut>(message: &str, work: Fut) -> T
where
    Fut: Future<Output = T>,
{
    if !is_pretty() || !is_interactive() {
        return work.await;
    }

    let mut work = std::pin::pin!(work);

    // Fast path: no spinner needed.
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), work.as_mut()).await {
        return result;
    }

    // Slow path: show spinner.
    hide_cursor();
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());
    pb.set_message(format!("{message}…"));
    pb.enable_steady_tick(Duration::from_millis(80));
    register_active_progress_bar(&pb);

    let result = work.await;
    clear_active_progress_bar();
    pb.finish_and_clear();
    show_cursor();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_spinner_runs_work_in_non_tty() {
        let result: Result<usize, String> = with_spinner("Loading", "Done", || Ok(42));
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn with_spinner_async_runs_future_in_non_tty_context() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let value: Result<usize, String> =
            rt.block_on(with_spinner_async("Working", "Done", async { Ok(42usize) }));
        assert_eq!(value.unwrap(), 42);
    }

    #[test]
    fn error_label_line_uses_single_space_before_elapsed() {
        assert_eq!(
            format_error_label_line("Connection failed", Some(Duration::from_secs(12))),
            "✘ Connection failed 12s"
        );
    }
}
