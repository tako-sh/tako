use console::{Term, measure_text_width};

pub(in crate::commands::dev) const RESET: &str = "\x1b[0m";
pub(in crate::commands::dev) const DIM: &str = "\x1b[2m";
#[allow(dead_code)]
const XDIM: &str = "\x1b[2;38;5;242m";
pub(super) const BORDER: &str = "\x1b[2;38;2;79;107;122m";

pub(super) const STACKED_THRESHOLD: usize = 76;
pub(super) const COL3_W: usize = 22;
pub(super) const COL_SEP: usize = 2;
pub(super) const BAR_W: usize = 8;
pub(super) const ROUTES_LABEL_W: usize = 8;
pub(super) const SCOPE_MIN: usize = 4;
pub(super) const SCOPE_MAX: usize = 32;

pub(super) fn muted(s: &str) -> String {
    format!("{DIM}{s}{RESET}")
}

pub(super) fn ansi_rgb(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

pub(super) fn split_route_pattern(route: &str) -> (&str, Option<&str>) {
    match route.find('/') {
        Some(idx) => (&route[..idx], Some(&route[idx..])),
        None => (route, None),
    }
}

pub(super) fn terminal_cols() -> usize {
    Term::stdout().size().1 as usize
}

pub(super) fn fmt_bytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{} MB", bytes / MB)
    }
}

pub(in crate::commands::dev) fn vlen(s: &str) -> usize {
    measure_text_width(s)
}

pub(in crate::commands::dev) fn progress_bar(fraction: f32, fill_width: usize) -> String {
    let f = fraction.clamp(0.0, 1.0);
    let filled = (f * fill_width as f32).round() as usize;
    let empty = fill_width.saturating_sub(filled);

    let (r, g, b) = if f < 0.5 {
        let t = f / 0.5;
        (
            (155.0 + t * 79.0) as u8,
            (217.0 - t * 6.0) as u8,
            (179.0 - t * 23.0) as u8,
        )
    } else {
        let t = (f - 0.5) / 0.5;
        (
            (234.0 - t * 2.0) as u8,
            (211.0 - t * 48.0) as u8,
            (156.0 + t * 4.0) as u8,
        )
    };

    let mut buf = String::with_capacity(fill_width * 20);
    if filled > 0 {
        buf.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
        for _ in 0..filled {
            buf.push('█');
        }
    }
    if empty > 0 {
        buf.push_str(DIM);
        for _ in 0..empty {
            buf.push('⣿');
        }
    }
    buf.push_str(RESET);
    buf
}

pub(super) fn status_dot(status: &str) -> (&'static str, &'static str) {
    match status {
        "running" => ("\x1b[38;2;155;217;179m", "●"),
        s if s.contains("launch") || s.contains("start") || s.contains("restart") => {
            ("\x1b[38;2;234;211;156m", "●")
        }
        "stopped" => ("\x1b[2m", "○"),
        "exited" => ("\x1b[38;2;232;163;160m", "●"),
        s if s.contains("error") => ("\x1b[38;2;232;163;160m", "●"),
        _ => ("\x1b[2m", "●"),
    }
}

fn fmt_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

pub(in crate::commands::dev) fn extract_repo_slug(url: &str) -> String {
    let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
    if !url.contains("://")
        && let Some(colon_pos) = url.find(':')
    {
        return url[colon_pos + 1..].to_string();
    }
    let parts: Vec<&str> = url.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        url.to_string()
    }
}

pub(in crate::commands::dev) fn git_info(
    dir: &std::path::Path,
) -> (String, String, String, Option<String>) {
    let dir_str = dir.to_string_lossy();

    let root_out = std::process::Command::new("git")
        .args(["-C", dir_str.as_ref(), "rev-parse", "--show-toplevel"])
        .output();

    let git_root = match root_out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => return (String::new(), String::new(), fmt_path(&dir_str), None),
    };

    let rel = dir
        .strip_prefix(&git_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let remote_out = std::process::Command::new("git")
        .args(["-C", dir_str.as_ref(), "remote", "get-url", "origin"])
        .output();

    let slug = match remote_out {
        Ok(out) if out.status.success() => extract_repo_slug(&String::from_utf8_lossy(&out.stdout)),
        _ => String::new(),
    };

    let branch = std::process::Command::new("git")
        .args(["-C", dir_str.as_ref(), "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let worktree_name = detect_worktree(dir);

    (slug, branch, rel, worktree_name)
}

fn detect_worktree(dir: &std::path::Path) -> Option<String> {
    let dir_str = dir.to_string_lossy();

    let toplevel = std::process::Command::new("git")
        .args(["-C", dir_str.as_ref(), "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !toplevel.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&toplevel.stdout).trim().to_string();
    let dot_git = std::path::Path::new(&root).join(".git");

    if dot_git.is_file() {
        let folder = std::path::Path::new(&root)
            .file_name()?
            .to_string_lossy()
            .to_string();
        Some(folder)
    } else {
        None
    }
}
