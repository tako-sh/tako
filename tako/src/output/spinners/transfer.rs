#![allow(dead_code)]

use std::time::{Duration, Instant};

use indicatif::ProgressBar;

use super::spinner_style;
use crate::output::cursor::{
    clear_active_progress_bar, hide_cursor, register_active_progress_bar, show_cursor,
};
use crate::output::{
    ACCENT, PHASE_PB, emit, format_elapsed_inline, format_size, is_interactive, is_pretty,
    should_colorize, theme_fg, theme_muted, theme_success,
};

// ---------------------------------------------------------------------------
// Transfer progress — single-line download/upload bar
// ---------------------------------------------------------------------------

/// Bar width in characters (for block bar).
const BAR_WIDTH: usize = 16;

/// Single-line transfer progress:
///
/// ```text
/// ⣼ Downloading…  ████████████░░░░  74%  1.2 MB/s  12s
/// ```
///
/// On completion:
///
/// ```text
/// ✔ Download complete 12s, 72 MB
/// ```
pub struct TransferProgress {
    pb: Option<ProgressBar>,
    start: Instant,
    loading_label: String,
    success_msg: String,
    total: u64,
    finished: std::sync::atomic::AtomicBool,
}

impl TransferProgress {
    /// Create a new transfer progress bar.
    ///
    /// - `loading` — verb phrase shown while in progress, e.g. `"Downloading"`
    /// - `success` — message shown on finish, e.g. `"Downloaded"`
    /// - `total` — total byte count (0 if unknown)
    pub fn new(loading: &str, success: &str, total: u64) -> Self {
        let start = Instant::now();
        let label = format!("{loading}…");
        // When a PhaseSpinner is active, skip creating our own spinner bar —
        // the finish() method will print above the phase spinner via emit().
        let pb = if PHASE_PB.lock().unwrap().is_some() {
            None
        } else if is_pretty() && is_interactive() {
            let pb = ProgressBar::new_spinner();
            pb.set_style(spinner_style());
            pb.set_message(format_transfer_progress_message(
                &label,
                0,
                total,
                Duration::ZERO,
            ));
            pb.enable_steady_tick(Duration::from_millis(80));
            hide_cursor();
            register_active_progress_bar(&pb);
            Some(pb)
        } else {
            None
        };
        Self {
            pb,
            start,
            loading_label: label,
            success_msg: success.to_string(),
            total,
            finished: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Update bytes transferred. Call this from the transfer loop.
    pub fn set_position(&self, bytes: u64) {
        if let Some(ref pb) = self.pb {
            let elapsed = self.start.elapsed();
            pb.set_message(format_transfer_progress_message(
                &self.loading_label,
                bytes,
                self.total,
                elapsed,
            ));
        }
    }

    /// Finish with success — shows `✔ <success_msg> <time>, <size>`.
    ///
    /// In pretty mode the progress bar is cleared so only the single summary
    /// line remains in scrollback.
    pub fn finish(&self) {
        if self
            .finished
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let elapsed = self.start.elapsed();
        let line = format_transfer_success_line(&self.success_msg, self.total, elapsed);

        if let Some(ref pb) = self.pb {
            clear_active_progress_bar();
            pb.finish_and_clear();
            show_cursor();
            emit(&line);
        } else if is_pretty() {
            // No own spinner (phase spinner was active) — emit above it.
            emit(&line);
        }
        // In verbose/CI mode: silent. The caller's `output::timed()` owns tracing.
    }
}

impl Drop for TransferProgress {
    fn drop(&mut self) {
        if !*self.finished.get_mut()
            && let Some(ref pb) = self.pb
        {
            clear_active_progress_bar();
            pb.finish_and_clear();
            show_cursor();
        }
    }
}

/// A spinner whose message can be updated while running.
/// Does NOT suppress other output (unlike PhaseSpinner).
///
/// In verbose/CI mode: silent — the caller's `output::timed()` owns
/// action tracing.
pub struct TrackedSpinner {
    pb: Option<ProgressBar>,
}

impl TrackedSpinner {
    pub fn start(message: &str) -> Self {
        if !is_pretty() {
            return Self { pb: None };
        }
        let pb = if is_interactive() {
            let pb = ProgressBar::new_spinner();
            pb.set_style(spinner_style());
            pb.set_message(message.to_string());
            pb.enable_steady_tick(Duration::from_millis(80));
            hide_cursor();
            register_active_progress_bar(&pb);
            Some(pb)
        } else {
            None
        };
        Self { pb }
    }

    pub fn set_message(&self, message: &str) {
        if let Some(ref pb) = self.pb {
            pb.set_message(message.to_string());
        }
    }

    pub fn finish(&self) {
        if let Some(ref pb) = self.pb {
            clear_active_progress_bar();
            pb.finish_and_clear();
            show_cursor();
        }
    }
}

impl Drop for TrackedSpinner {
    fn drop(&mut self) {
        if let Some(ref pb) = self.pb {
            clear_active_progress_bar();
            pb.finish_and_clear();
            show_cursor();
        }
    }
}

/// Compact transfer detail for ratatui task trees (no block bar — the bar is
/// rendered natively by the UI layer via `TaskItemState::progress`).
pub fn format_transfer_compact_detail(bytes: u64, total: u64, _elapsed: Duration) -> String {
    let fraction = transfer_progress_fraction(bytes, total);
    let pct = format!("{:.0}%", fraction * 100.0);
    let amount = format_transfer_amount(bytes, total);
    format!("{pct}, {amount}")
}

fn format_transfer_progress_message(
    label: &str,
    bytes: u64,
    total: u64,
    elapsed: Duration,
) -> String {
    let parts = transfer_progress_detail_parts(bytes, total, elapsed);
    format!("{label}  {}", parts.join("  "))
}

fn transfer_progress_detail_parts(bytes: u64, total: u64, elapsed: Duration) -> Vec<String> {
    let fraction = transfer_progress_fraction(bytes, total);
    let bar = render_block_bar(fraction);
    let pct = format!("{:.0}%", fraction * 100.0);
    let time = format_elapsed_inline(elapsed);
    let amount = format_transfer_amount(bytes, total);
    let mut parts = Vec::new();
    if !time.is_empty() {
        parts.push(time);
    }
    parts.push(bar);
    parts.push(pct);
    parts.push(amount);
    parts
}

fn transfer_progress_fraction(bytes: u64, total: u64) -> f64 {
    if total > 0 {
        (bytes as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Render a block progress bar: filled `█` in accent color, empty `░` dimmed.
fn render_block_bar(fraction: f64) -> String {
    let (filled, empty) = block_bar_segments(fraction);

    let mut buf = String::with_capacity(BAR_WIDTH * 10);
    let colorize = should_colorize();

    if filled > 0 {
        if colorize {
            let (r, g, b) = ACCENT;
            buf.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
        }
        for _ in 0..filled {
            buf.push('█');
        }
    }

    if empty > 0 {
        if colorize {
            buf.push_str("\x1b[2m");
        }
        for _ in 0..empty {
            buf.push('░');
        }
    }

    if colorize {
        buf.push_str("\x1b[0m");
    }
    buf
}

fn block_bar_segments(fraction: f64) -> (usize, usize) {
    let f = fraction.clamp(0.0, 1.0);
    let filled = (f * BAR_WIDTH as f64).round() as usize;
    let empty = BAR_WIDTH.saturating_sub(filled);
    (filled, empty)
}

fn format_transfer_amount(bytes: u64, total: u64) -> String {
    if total > 0 {
        format!("({}/{})", format_size(bytes), format_size(total))
    } else {
        format!("({})", format_size(bytes))
    }
}

fn format_transfer_success_line(success_msg: &str, total: u64, elapsed: Duration) -> String {
    let check = theme_success("✔");
    let mut details = Vec::new();
    let time = format_elapsed_inline(elapsed);
    if !time.is_empty() {
        details.push(time);
    }
    if total > 0 {
        details.push(format_size(total));
    }

    if details.is_empty() {
        format!("{check} {}", theme_fg(success_msg))
    } else {
        format!(
            "{check} {} {}",
            theme_fg(success_msg),
            theme_muted(details.join(", "))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_progress_in_non_tty() {
        let tp = TransferProgress::new("Downloading", "Downloaded", 1024);
        tp.set_position(512);
        tp.finish();
    }

    #[test]
    fn format_transfer_compact_detail_omits_block_bar() {
        assert_eq!(
            format_transfer_compact_detail(512, 1024, Duration::from_secs(2)),
            "50%, (512 bytes/1.00 KB)"
        );
    }

    #[test]
    fn transfer_success_line_uses_single_space_before_elapsed() {
        assert_eq!(
            format_transfer_success_line(
                "Download complete",
                72 * 1024 * 1024,
                Duration::from_secs(12)
            ),
            "✔ Download complete 12s, 72.00 MB"
        );
    }
}
