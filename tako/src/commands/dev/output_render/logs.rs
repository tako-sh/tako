use super::super::{LogLevel, ScopedLog};
use super::shared::{DIM, RESET, SCOPE_MAX, SCOPE_MIN, ansi_rgb, muted};

pub(in crate::commands::dev) fn fit_scope(scope: &str) -> String {
    let len = scope.len();
    if len <= SCOPE_MAX {
        format!("{scope:<SCOPE_MIN$}")
    } else {
        format!("{}\u{2026}", &scope[..SCOPE_MAX - 1])
    }
}

pub(in crate::commands::dev) fn format_log(log: &ScopedLog) -> String {
    if let Some(kind) = log.kind.as_deref() {
        let label = kind.replace('_', " ");
        return muted(&format!("──── {label} ────"));
    }
    if let Some(ip) = log
        .scope
        .eq("tako")
        .then(|| log.message.strip_prefix("LAN mode enabled ("))
        .flatten()
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let color = level_color(&log.level);
        let scope = fit_scope(&log.scope);
        let rendered_scope = render_scope(&log.scope, &scope);
        return format!(
            "{DIM}{}{RESET} {color}{:>5}{RESET} {rendered_scope} LAN mode enabled {DIM}({ip}){RESET}",
            log.timestamp, log.level
        );
    }
    let fields_suffix = format_fields_suffix(log.fields.as_ref());
    if matches!(log.level, LogLevel::Debug) {
        let scope = fit_scope(&log.scope);
        let rendered_scope = render_scope(&log.scope, &scope);
        let pad_width = message_column_width(scope.len());
        let mut lines = log.message.split('\n');
        let first = lines.next().unwrap_or("");
        let mut out = format!(
            "{DIM}{} {:>5}{RESET} {rendered_scope} {DIM}{first}{fields_suffix}{RESET}",
            log.timestamp, log.level
        );
        for line in lines {
            out.push('\n');
            out.push_str(&" ".repeat(pad_width));
            out.push_str(&format!("{DIM}{line}{RESET}"));
        }
        return out;
    }
    let color = level_color(&log.level);
    let scope = fit_scope(&log.scope);
    let rendered_scope = render_scope(&log.scope, &scope);
    let pad_width = message_column_width(scope.len());
    let mut lines = log.message.split('\n');
    let first = lines.next().unwrap_or("");
    let mut out = format!(
        "{DIM}{}{RESET} {color}{:>5}{RESET} {rendered_scope} {first}{fields_suffix}",
        log.timestamp, log.level
    );
    for line in lines {
        out.push('\n');
        out.push_str(&" ".repeat(pad_width));
        out.push_str(line);
    }
    out
}

/// Render `fields` as a dim trailing ` key=value` suffix. Skips globals that
/// are constant per-process (`build`, `instance`). Object values with a
/// `message` field (e.g. Errors) render as just their message. Returns empty
/// string when there are no visible fields.
pub(super) fn format_fields_suffix(
    fields: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
    let Some(map) = fields else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in map {
        if matches!(k.as_str(), "build" | "instance") {
            continue;
        }
        parts.push(format!("{k}={}", render_field_value(v)));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(" {DIM}{}{RESET}", parts.join(" "))
}

fn render_field_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Object(obj) => {
            // Errors serialize to { name, message, stack } — collapse to message.
            if let Some(msg) = obj.get("message").and_then(|m| m.as_str()) {
                return msg.to_string();
            }
            serde_json::to_string(v).unwrap_or_default()
        }
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Visible width of the timestamp/level/scope prefix up to where the message
/// starts, used to indent continuation lines of multi-line messages so they
/// align under the first line's message column.
fn message_column_width(scope_visible_width: usize) -> usize {
    // "HH:MM:SS" (8) + " " + level right-aligned to 5 + " " + scope + " "
    8 + 1 + 5 + 1 + scope_visible_width + 1
}

fn level_color(level: &LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "\x1b[38;2;140;207;255m",
        LogLevel::Info => "\x1b[38;2;155;217;179m",
        LogLevel::Warn => "\x1b[38;2;234;211;156m",
        LogLevel::Error => "\x1b[38;2;232;163;160m",
        LogLevel::Fatal => "\x1b[38;2;200;166;242m",
    }
}

const SCOPE_PALETTE: &[(u8, u8, u8)] = &[
    (138, 198, 209),
    (194, 178, 128),
    (176, 186, 140),
    (190, 168, 206),
    (140, 195, 174),
    (209, 170, 160),
    (160, 190, 210),
    (200, 180, 170),
];

static APP_RUNTIME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the project runtime (e.g. "bun", "node", "go") so the "app"
/// scope can be tinted with the runtime's brand color. Idempotent; first call wins.
pub(in crate::commands::dev) fn set_app_runtime(runtime: impl Into<String>) {
    let _ = APP_RUNTIME.set(runtime.into());
}

fn app_runtime() -> Option<&'static str> {
    APP_RUNTIME.get().map(String::as_str)
}

/// Render the padded scope label with ANSI color. Known scopes get gradients;
/// everything else falls back to a hash-derived solid color from the palette.
/// Compound scopes like `worker:broadcast` get a split rendering: the prefix
/// keeps a stable color per subsystem, the `:` separator is dimmed, and the
/// suffix gets its own color so different workflows are visually distinct.
fn render_scope(raw: &str, padded: &str) -> String {
    if raw.len() <= SCOPE_MAX
        && let Some((prefix, suffix)) = raw.split_once(':')
        && !suffix.is_empty()
    {
        let padding = &padded[raw.len()..];
        let (pr, pg, pb) = scope_solid(prefix);
        let (sr, sg, sb) = scope_solid(suffix);
        return format!(
            "{}{prefix}{RESET}{DIM}:{RESET}{}{suffix}{RESET}{padding}",
            ansi_rgb(pr, pg, pb),
            ansi_rgb(sr, sg, sb),
        );
    }
    if let Some(stops) = scope_gradient(raw) {
        return apply_gradient(padded, raw, stops);
    }
    let (r, g, b) = scope_solid(raw);
    format!("{}{padded}{RESET}", ansi_rgb(r, g, b))
}

fn scope_gradient(scope: &str) -> Option<&'static [(u8, u8, u8)]> {
    match scope {
        "tako" => Some(&[(232, 135, 131), (240, 195, 160)]),
        "vite" => Some(&[(143, 90, 200), (189, 132, 230)]),
        "app" => app_runtime().and_then(runtime_gradient),
        _ => None,
    }
}

fn runtime_gradient(runtime: &str) -> Option<&'static [(u8, u8, u8)]> {
    match runtime {
        "bun" => Some(&[(251, 240, 223), (244, 113, 181)]),
        "node" => Some(&[(60, 135, 58), (140, 200, 75)]),
        "go" => Some(&[(0, 173, 216), (93, 201, 226)]),
        _ => None,
    }
}

fn scope_solid(scope: &str) -> (u8, u8, u8) {
    match scope {
        "app" => (200, 200, 190),
        "worker" => (140, 205, 195),
        _ => {
            let hash = scope
                .bytes()
                .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
            SCOPE_PALETTE[hash as usize % SCOPE_PALETTE.len()]
        }
    }
}

/// Apply per-character gradient interpolation across the visible chars of the
/// scope name. Padding chars (after the raw name) are emitted uncolored.
fn apply_gradient(padded: &str, raw: &str, stops: &[(u8, u8, u8)]) -> String {
    let visible = raw.chars().count();
    let mut out = String::with_capacity(padded.len() + visible * 20);
    for (i, ch) in padded.chars().enumerate() {
        if i < visible {
            let t = if visible > 1 {
                i as f32 / (visible - 1) as f32
            } else {
                0.0
            };
            let (r, g, b) = sample_stops(stops, t);
            out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
            out.push(ch);
        } else {
            out.push(ch);
        }
    }
    out.push_str(RESET);
    out
}

fn sample_stops(stops: &[(u8, u8, u8)], t: f32) -> (u8, u8, u8) {
    if stops.len() == 1 {
        return stops[0];
    }
    let scaled = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
    let idx = (scaled.floor() as usize).min(stops.len() - 2);
    let local = scaled - idx as f32;
    let (r0, g0, b0) = stops[idx];
    let (r1, g1, b1) = stops[idx + 1];
    (
        lerp_u8(r0, r1, local),
        lerp_u8(g0, g1, local),
        lerp_u8(b0, b1, local),
    )
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}
