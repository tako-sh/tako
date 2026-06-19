pub mod cursor;
mod prompts;
pub mod spinners;
mod tracing_fmt;

// Re-export prompt types and functions
pub use prompts::{
    TextField, Wizard, confirm, confirm_with_description, is_operation_cancelled_error,
    is_silent_exit_error, is_wizard_back, operation_cancelled, operation_cancelled_error,
    password_field, select, silent_exit_error,
};

// Re-export spinner types and functions
pub use spinners::{
    PhaseSpinner, SPINNER_TICKS, TrackedSpinner, with_spinner, with_spinner_async,
    with_spinner_async_err, with_spinner_async_simple, with_spinner_silent,
};

// Re-export tracing types
pub use tracing_fmt::{ScopeFormat, ScopeLayer, scope, timed};

// Re-export cursor management
pub use cursor::{clear_interrupt_output, restore_cursor, set_cursor_globally_hidden};

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use console::Term;
use indicatif::ProgressBar;

static VERBOSE: AtomicBool = AtomicBool::new(false);
static CI: AtomicBool = AtomicBool::new(false);
static DRY_RUN: AtomicBool = AtomicBool::new(false);
static JSON: AtomicBool = AtomicBool::new(false);

/// Active PhaseSpinner's ProgressBar. When set, all output routes through
/// `pb.println()` so the spinner stays on the last line.
static PHASE_PB: Mutex<Option<ProgressBar>> = Mutex::new(None);

/// Print a line to stderr, routing through the active PhaseSpinner if one exists.
// CodeQL[rust/cleartext-logging]: UI chrome only; password fields mask input with "•" before output
fn emit(line: &str) {
    if let Some(pb) = PHASE_PB.lock().unwrap().as_ref() {
        pb.println(line);
    } else {
        eprintln!("{line}");
    }
}

// ── Brand palette ──────────────────────────────────────────────────────────

const THEME_TEAL: (u8, u8, u8) = (155, 196, 182); // #9BC4B6
const THEME_CORAL: (u8, u8, u8) = (232, 135, 131); // #E88783
const THEME_GREEN: (u8, u8, u8) = (155, 217, 179); // #9BD9B3 — success
const THEME_AMBER: (u8, u8, u8) = (234, 211, 156); // #EAD39C — warning
const THEME_RED: (u8, u8, u8) = (232, 163, 160); // #E8A3A0 — error

// Terminal accent colors (distinct from brand palette)
const ACCENT: (u8, u8, u8) = (125, 196, 228); // #7DC4E4

fn should_colorize() -> bool {
    if cfg!(test) {
        return false;
    }
    !is_ci() && std::io::stderr().is_terminal()
}

fn rgb_fg<D: Display>(value: D, (r, g, b): (u8, u8, u8)) -> String {
    if should_colorize() {
        format!("\x1b[38;2;{r};{g};{b}m{value}\x1b[39m")
    } else {
        value.to_string()
    }
}

pub fn theme_accent<D: Display>(value: D) -> String {
    rgb_fg(value, ACCENT)
}

pub fn theme_fg<D: Display>(value: D) -> String {
    value.to_string()
}

pub fn theme_muted<D: Display>(value: D) -> String {
    if should_colorize() {
        let s = value.to_string().replace("\x1b[22m", "\x1b[22m\x1b[2m");
        format!("\x1b[2m{s}\x1b[22m")
    } else {
        value.to_string()
    }
}

pub fn theme_dim<D: Display>(value: D) -> String {
    if should_colorize() {
        format!("\x1b[38;2;100;100;100m{value}\x1b[39m")
    } else {
        value.to_string()
    }
}

pub fn theme_success<D: Display>(value: D) -> String {
    rgb_fg(value, THEME_GREEN)
}

pub fn theme_warning<D: Display>(value: D) -> String {
    rgb_fg(value, THEME_AMBER)
}

pub fn theme_error<D: Display>(value: D) -> String {
    rgb_fg(value, THEME_RED)
}

fn bold(value: &str) -> String {
    if should_colorize() {
        format!("\x1b[1m{value}\x1b[22m")
    } else {
        value.to_string()
    }
}

pub fn underline<D: Display>(value: D) -> String {
    if should_colorize() {
        format!("\x1b[4m{value}\x1b[24m")
    } else {
        value.to_string()
    }
}

// ── Elapsed time formatting ────────────────────────────────────────────────

pub fn format_elapsed(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 0.1 {
        String::new()
    } else if secs < 10.0 {
        format!("{:.1}s", secs)
    } else if secs < 60.0 {
        format!("{}s", secs as u64)
    } else {
        let mins = secs as u64 / 60;
        let remaining = secs as u64 % 60;
        format!("{}m{}s", mins, remaining)
    }
}

/// Like `format_elapsed` but shows sub-100ms durations too.
pub fn format_elapsed_always(duration: Duration) -> String {
    if duration < Duration::from_millis(50) {
        return String::new();
    }
    let secs = duration.as_secs_f64();
    if secs < 10.0 {
        format!("{:.1}s", secs)
    } else if secs < 60.0 {
        format!("{}s", secs as u64)
    } else {
        let mins = secs as u64 / 60;
        let remaining = secs as u64 % 60;
        format!("{}m{}s", mins, remaining)
    }
}

/// Format elapsed for TRACE log lines. Always shows a value (even sub-100ms).
pub fn format_elapsed_trace(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("({ms}ms)")
    } else {
        let secs = duration.as_secs_f64();
        if secs < 10.0 {
            format!("({:.1}s)", secs)
        } else if secs < 60.0 {
            format!("({}s)", secs as u64)
        } else {
            let mins = secs as u64 / 60;
            let remaining = secs as u64 % 60;
            format!("({}m{}s)", mins, remaining)
        }
    }
}

/// Format elapsed for inline spinner display, e.g. `"1m10s"`.
fn format_elapsed_inline(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let remaining = secs % 60;
        format!("{mins}m{remaining}s")
    }
}

/// Format a muted elapsed-time string, e.g. `"(3.2s)"` rendered in muted style.
pub fn muted_elapsed(duration: Duration) -> String {
    let s = format_elapsed(duration);
    if s.is_empty() { s } else { theme_muted(&s) }
}

fn format_elapsed_suffix(elapsed: Duration) -> String {
    let elapsed = muted_elapsed(elapsed);
    if elapsed.is_empty() {
        String::new()
    } else {
        format!(" {elapsed}")
    }
}

fn format_success_elapsed_line(message: &str, elapsed: Duration) -> String {
    format!(
        "{} {}{}",
        theme_success("✔"),
        theme_fg(message),
        format_elapsed_suffix(elapsed)
    )
}

/// Format a muted progress counter, e.g. `"[2/5]"` rendered in muted style.
pub fn muted_progress(done: usize, total: usize) -> String {
    theme_muted(format!("[{done}/{total}]"))
}

/// Format a byte count as a human-readable size string.
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

// ── Mode flags ─────────────────────────────────────────────────────────────

pub fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

pub fn set_ci(ci: bool) {
    CI.store(ci, Ordering::Relaxed);
}

pub fn set_json(json: bool) {
    JSON.store(json, Ordering::Relaxed);
}

pub fn set_dry_run(dry_run: bool) {
    DRY_RUN.store(dry_run, Ordering::Relaxed);
}

pub fn is_dry_run() -> bool {
    DRY_RUN.load(Ordering::Relaxed)
}

pub fn is_interactive() -> bool {
    #[cfg(test)]
    {
        false
    }

    #[cfg(not(test))]
    {
        !is_ci() && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
}

pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn is_ci() -> bool {
    CI.load(Ordering::Relaxed)
}

pub fn is_json() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// True when running as root (euid 0), meaning sudo prompts are unnecessary.
#[cfg(unix)]
pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
pub fn is_root() -> bool {
    false
}

/// True when pretty output should render (normal interactive mode).
/// False in verbose or CI mode, where tracing handles all output.
pub fn is_pretty() -> bool {
    !is_verbose() && !is_ci() && !is_json()
}

pub fn json_success(command: &str) -> Result<(), Box<dyn std::error::Error>> {
    json_result(serde_json::json!({
        "ok": true,
        "command": command,
    }))
}

pub fn json_result(value: serde_json::Value) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

pub fn json_error(message: &str) -> Result<(), Box<dyn std::error::Error>> {
    json_result(serde_json::json!({
        "ok": false,
        "error": {
            "message": message,
        },
    }))
}

/// Print live stream output. Normal dev/log streaming keeps stdout for terminal
/// use; JSON mode reserves stdout for machine-readable command results.
pub fn stream_line(message: &str) {
    if is_json() {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

pub fn stream_blank_line() {
    if is_json() {
        eprintln!();
    } else {
        println!();
    }
}

// ── Logo ───────────────────────────────────────────────────────────────────

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_SHA: Option<&str> = option_env!("TAKO_BUILD_SHA");

pub const LOGO_ROWS: [&str; 3] = [
    "▀█▀ ▄▀█ █ █ █▀█   █▀ █ █",
    " █  █▀█ █▀▄ █ █   ▀█ █▀█",
    " ▀  ▀ ▀ ▀ ▀ ▀▀▀ ▀ ▀▀ ▀ ▀",
];

const LOGO_COLOR_START: (u8, u8, u8) = THEME_TEAL;
const LOGO_COLOR_END: (u8, u8, u8) = THEME_CORAL;

fn build_version_string() -> String {
    match BUILD_SHA {
        Some(sha) if !sha.trim().is_empty() => {
            let short = &sha[..sha.len().min(7)];
            format!("{VERSION}-{short}")
        }
        _ => VERSION.to_owned(),
    }
}

fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t).round() as u8
}

/// Compact logo with left-to-right gradient. Version shown next to first row.
pub fn format_logo_header() -> String {
    let version_str = format!("v{}", build_version_string());
    let char_count = LOGO_ROWS[0].chars().count();
    let colorize = should_colorize();
    let mut lines = Vec::new();
    for (i, row) in LOGO_ROWS.iter().enumerate() {
        let line = if colorize {
            let mut buf = String::from("  ");
            for (j, ch) in row.chars().enumerate() {
                let t = if char_count <= 1 {
                    0.0
                } else {
                    j as f64 / (char_count - 1) as f64
                };
                let r = lerp_u8(LOGO_COLOR_START.0, LOGO_COLOR_END.0, t);
                let g = lerp_u8(LOGO_COLOR_START.1, LOGO_COLOR_END.1, t);
                let b = lerp_u8(LOGO_COLOR_START.2, LOGO_COLOR_END.2, t);
                buf.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
                buf.push(ch);
            }
            buf.push_str("\x1b[0m");
            if i == 0 {
                buf.push_str(&format!("  \x1b[2m{version_str}\x1b[0m"));
            }
            buf
        } else {
            let mut buf = format!("  {row}");
            if i == 0 {
                buf.push_str(&format!("  {version_str}"));
            }
            buf
        };
        lines.push(line);
    }
    lines.join("\n")
}

/// Print the logo header to stderr.
pub fn logo_header() {
    if !is_pretty() {
        return;
    }
    emit("");
    for line in format_logo_header().lines() {
        emit(line);
    }
    emit("");
}

// ── Text output functions ──────────────────────────────────────────────────

pub fn section(title: &str) {
    if is_pretty() {
        emit("");
        emit(&padded(&bold(&theme_accent(title))));
    }
}

pub fn heading(title: &str) {
    if is_pretty() {
        emit("");
        emit(&padded(&bold(title)));
    }
}

pub fn info(message: &str) {
    if is_pretty() {
        emit(&padded(&theme_fg(message)));
    }
}

/// Like `info`, but skips the interactive 2-space indent. Use for isolated
/// summary blocks that render on their own (no co-located spinners or
/// symbol-prefixed lines to align with).
pub fn line(message: &str) {
    if is_pretty() {
        emit(&theme_fg(message));
    }
}

pub fn bullet(message: &str) {
    if is_pretty() {
        emit(&format!("  - {}", theme_fg(message)));
    }
}

fn format_warning_full_line(message: &str) -> String {
    format!(
        "{} {}",
        theme_warning(theme_muted("┃")),
        theme_warning(message)
    )
}

fn format_warning_bullet_line(message: &str) -> String {
    format!(
        "{} {}",
        theme_warning(theme_muted("┃")),
        theme_warning(format!("• {message}"))
    )
}

pub fn success(message: &str) {
    if is_pretty() {
        emit(&format!("{} {}", theme_success("✔"), theme_fg(message)));
    } else {
        tracing::info!("{}", message);
    }
}

pub fn success_with_elapsed(message: &str, elapsed: Duration) {
    let time = format_elapsed(elapsed);
    if is_pretty() {
        emit(&format_success_elapsed_line(message, elapsed));
    } else if time.is_empty() {
        tracing::info!("{}", message);
    } else {
        tracing::info!("{} {}", message, time);
    }
}

pub fn warning(message: &str) {
    if is_pretty() {
        emit(&format!(
            "{} {}",
            bold(&theme_warning("!")),
            theme_warning(message)
        ));
    }
}

#[allow(dead_code)]
pub fn warning_full(message: &str) {
    if is_pretty() {
        emit(&format_warning_full_line(message));
    }
}

#[allow(dead_code)]
pub fn warning_bullet(message: &str) {
    if is_pretty() {
        emit(&format_warning_bullet_line(message));
    }
}

pub fn error(message: &str) {
    error_block(message);
}

/// Always prints — used for fatal errors in main.rs.
pub fn error_stderr(message: &str) {
    if !is_pretty() {
        tracing::error!("{}", message);
        return;
    }
    eprintln!("{} {}", bold(&theme_error("✘")), theme_fg(message));
}

/// Print a wrapped red error message without prompt chrome.
pub fn error_block(message: &str) {
    if !is_pretty() {
        tracing::error!("{}", message);
        return;
    }
    emit(&format_error_block(message));
}

/// Error block used during interactive prompt validation.
pub(crate) fn format_error_block(message: &str) -> String {
    let term_cols = Term::stderr().size().1 as usize;
    let width = if term_cols > 0 { term_cols } else { 80 };
    let lines = wrap_text(message, width);

    lines.iter().map(theme_error).collect::<Vec<_>>().join("\n")
}

/// Wrap `text` into lines no wider than `max_width` visible chars.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split_whitespace() {
        let word_width = console::measure_text_width(word);
        if current_width == 0 {
            if word_width > max_width {
                for ch in word.chars() {
                    let cw = console::measure_text_width(&ch.to_string());
                    if current_width + cw > max_width {
                        lines.push(current.clone());
                        current.clear();
                        current_width = 0;
                    }
                    current.push(ch);
                    current_width += cw;
                }
            } else {
                current.push_str(word);
                current_width = word_width;
            }
        } else if current_width + 1 + word_width <= max_width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(current.clone());
            current = word.to_string();
            current_width = word_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Print a dry-run skip notice: "⏭ {message} (dry run)"
pub fn dry_run_skip(message: &str) {
    if is_pretty() {
        emit(&format!(
            "{} {} {}",
            theme_muted("⏭"),
            theme_fg(message),
            theme_muted("(dry run)")
        ));
    } else if is_ci() {
        emit(&format_dry_run_skip_plain(message));
    } else {
        tracing::info!("dry-run skip: {}", message);
    }
}

fn format_dry_run_skip_plain(message: &str) -> String {
    format!("⏭ {message} (dry-run)")
}

pub fn muted(message: &str) {
    if is_pretty() {
        emit(&padded(&theme_muted(message)));
    }
}

/// Print a hint line in default text color (not muted).
pub fn hint(message: &str) {
    if is_pretty() {
        emit(&padded(&theme_dim(message)));
    } else {
        tracing::info!("{}", message);
    }
}

/// Indentation prefix for lines under a heading (2 spaces).
pub const INDENT: &str = "  ";

/// Diamond symbols for interactive prompts.
pub const DIAMOND_FILLED: &str = "◆";
pub const DIAMOND_OUTLINED: &str = "◇";
pub const OPERATION_CANCELLED: &str = "Operation cancelled";

/// In interactive pretty mode, prepend INDENT so plain text aligns with
/// symbol-prefixed lines (`✔`/`✘`/`⠋` already occupy 2 chars + space).
fn padded(line: &str) -> String {
    if is_pretty() && is_interactive() {
        format!("{INDENT}{line}")
    } else {
        line.to_string()
    }
}

/// Bold only (no color). The one thing you want the eye to catch.
pub fn strong(value: &str) -> String {
    bold(value)
}

/// Accent color only (no bold). Secondary emphasis.
#[allow(dead_code)]
pub fn accent(value: &str) -> String {
    theme_accent(value)
}

#[cfg(test)]
mod tests;
