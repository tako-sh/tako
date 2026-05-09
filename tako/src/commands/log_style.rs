pub(crate) const DIM: &str = "\x1b[2m";
pub(crate) const RESET: &str = "\x1b[0m";
const META: &str = "\x1b[2;3m";

pub(crate) fn ansi_rgb(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
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

pub(crate) fn set_app_runtime(runtime: impl Into<String>) {
    let _ = APP_RUNTIME.set(runtime.into());
}

fn app_runtime() -> Option<&'static str> {
    APP_RUNTIME.get().map(String::as_str)
}

/// Render a scope label with ANSI color. Known scopes get gradients; everything
/// else gets a stable hash-derived color from the shared log palette.
pub(crate) fn render_scope(raw: &str, padded: &str) -> String {
    if raw.len() <= padded.len()
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

pub(crate) fn render_compact_scope(raw: &str) -> String {
    render_scope(raw, raw)
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

pub(crate) fn format_json_fields_suffix(
    fields: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
    let Some(map) = fields else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in map {
        if matches!(key.as_str(), "build" | "instance") {
            continue;
        }
        parts.push(format!("{key}={}", render_field_value(value)));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(" {META}{}{RESET}", parts.join(" "))
}

pub(crate) fn render_metadata_suffix(fields: &str, colorize: bool) -> String {
    if fields.is_empty() {
        String::new()
    } else if colorize {
        format!(" {META}{fields}{RESET}")
    } else {
        format!(" {fields}")
    }
}

fn render_field_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Object(object) => {
            if let Some(message) = object.get("message").and_then(|value| value.as_str()) {
                return message.to_string();
            }
            serde_json::to_string(value).unwrap_or_default()
        }
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}
