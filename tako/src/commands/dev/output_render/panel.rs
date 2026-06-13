use console::{measure_text_width, truncate_str};

use super::shared::{
    BAR_W, BORDER, COL_SEP, COL3_W, DIM, RESET, ROUTES_LABEL_W, STACKED_THRESHOLD, ansi_rgb,
    fmt_bytes, muted, progress_bar, status_dot, terminal_cols, vlen,
};

pub(in crate::commands::dev) fn format_header() -> String {
    crate::output::format_logo_header()
}

/// Build the panel title. Returns (visible_text, rendered_text); the visible
/// form is used for column-width math, the rendered form carries ANSI styling.
/// Combines the repo slug and folder path into a single locator so monorepo
/// subprojects are unambiguous.
fn panel_title(app_name: &str, repo_slug: &str, repo_path: &str) -> (String, String) {
    let locator = match (repo_slug.is_empty(), repo_path.is_empty()) {
        (true, true) => return (app_name.to_string(), app_name.to_string()),
        (false, true) => repo_slug.to_string(),
        (true, false) => repo_path.to_string(),
        (false, false) => format!("{repo_slug}/{repo_path}"),
    };
    (
        format!("{app_name} ({locator})"),
        format!("{app_name} {DIM}({locator}){RESET}"),
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::commands::dev) fn format_panel(
    app_name: &str,
    status: &str,
    adapter_name: &str,
    repo_slug: &str,
    repo_branch: &str,
    repo_path: &str,
    worktree_name: Option<&str>,
    hosts: &[String],
    port: u16,
    cpu: Option<f32>,
    mem_bytes: Option<u64>,
) -> String {
    let cols = terminal_cols().max(40);
    if cols < STACKED_THRESHOLD {
        format_panel_stacked(
            app_name,
            status,
            adapter_name,
            repo_slug,
            repo_branch,
            repo_path,
            worktree_name,
            hosts,
            port,
            cpu,
            mem_bytes,
            cols,
        )
    } else {
        format_panel_wide(
            app_name,
            status,
            adapter_name,
            repo_slug,
            repo_branch,
            repo_path,
            worktree_name,
            hosts,
            port,
            cpu,
            mem_bytes,
            cols,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::commands::dev) fn format_panel_wide(
    app_name: &str,
    status: &str,
    _adapter_name: &str,
    repo_slug: &str,
    repo_branch: &str,
    repo_path: &str,
    worktree_name: Option<&str>,
    hosts: &[String],
    port: u16,
    cpu: Option<f32>,
    mem_bytes: Option<u64>,
    cols: usize,
) -> String {
    let url_color = ansi_rgb(240, 175, 95);

    let urls: Vec<String> = hosts
        .iter()
        .map(|h| {
            if port == 443 {
                format!("https://{h}")
            } else {
                format!("https://{h}:{port}")
            }
        })
        .collect();

    let inner_w = cols.saturating_sub(2);
    let shared = inner_w.saturating_sub(2 + COL3_W + 2 * COL_SEP);
    let col1_w = (shared / 3).max(10);
    let col2_w = shared.saturating_sub(col1_w).max(10);

    let (title_visible, title_render) = panel_title(app_name, repo_slug, repo_path);
    let title_seg = format!("─ {title_visible} ");
    let tail = inner_w.saturating_sub(measure_text_width(&title_seg));
    let top = format!(
        "{BORDER}┌─ {RESET}{title_render}{BORDER} {}┐{RESET}",
        "─".repeat(tail)
    );
    let bot = format!("{BORDER}└{}┘{RESET}", "─".repeat(inner_w));

    let (dot_color, dot_char) = status_dot(status);
    let l0 = format!("{dot_color}{dot_char}{RESET} {status}");
    let mut left = vec![l0];
    if let Some(wt) = worktree_name {
        let wt_label = format!("worktree ({wt})");
        let wt_t = truncate_str(&wt_label, col1_w, "…");
        left.push(muted(&wt_t));
    }
    if !repo_branch.is_empty() {
        left.push(format!("{} {repo_branch}", muted("\u{e0a0}")));
    }

    let url_avail = col2_w.saturating_sub(ROUTES_LABEL_W);
    let mid: Vec<String> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let url_t = truncate_str(url, url_avail, "…");
            if i == 0 {
                format!("{}  {url_color}{url_t}{RESET}", muted("routes"))
            } else {
                format!("{}{url_color}{url_t}{RESET}", " ".repeat(ROUTES_LABEL_W))
            }
        })
        .collect();

    let r0 = if let Some(c) = cpu {
        let bar = progress_bar(c / 100.0, BAR_W);
        format!("{}  {bar} {:.0}%", muted("cpu"), c)
    } else {
        format!("{}  —", muted("cpu"))
    };
    let r1 = if let Some(m) = mem_bytes {
        format!("{}  {}", muted("ram"), fmt_bytes(m))
    } else {
        format!("{}  —", muted("ram"))
    };
    let right = [r0, r1];

    let data_rows = left.len().max(mid.len()).max(right.len());
    let mut lines = vec![top];
    for i in 0..data_rows {
        let la = left.get(i).map(|s| s.as_str()).unwrap_or("");
        let ma = mid.get(i).map(|s| s.as_str()).unwrap_or("");
        let ra = right.get(i).map(|s| s.as_str()).unwrap_or("");
        lines.push(panel_row(la, ma, ra, col1_w, col2_w));
    }
    lines.push(bot);
    lines.join("\n")
}

#[allow(clippy::too_many_arguments)]
pub(in crate::commands::dev) fn format_panel_stacked(
    app_name: &str,
    status: &str,
    _adapter_name: &str,
    repo_slug: &str,
    repo_branch: &str,
    repo_path: &str,
    worktree_name: Option<&str>,
    hosts: &[String],
    port: u16,
    cpu: Option<f32>,
    mem_bytes: Option<u64>,
    cols: usize,
) -> String {
    let url_color = ansi_rgb(240, 175, 95);
    let inner_w = cols.saturating_sub(2);

    let (title_visible, title_render) = panel_title(app_name, repo_slug, repo_path);
    let title_seg = format!("─ {title_visible} ");
    let tail = inner_w.saturating_sub(measure_text_width(&title_seg));
    let top = format!(
        "{BORDER}┌─ {RESET}{title_render}{BORDER} {}┐{RESET}",
        "─".repeat(tail)
    );
    let bot = format!("{BORDER}└{}┘{RESET}", "─".repeat(inner_w));

    let mut rows = vec![top];

    let (dot_color, dot_char) = status_dot(status);
    rows.push(stacked_row(
        &format!("{dot_color}{dot_char}{RESET} {status}"),
        inner_w,
    ));

    let avail = inner_w.saturating_sub(2);
    if let Some(wt) = worktree_name {
        let wt_label = format!("worktree ({wt})");
        let wt_t = truncate_str(&wt_label, avail, "…");
        rows.push(stacked_row(&muted(&wt_t), inner_w));
    }

    if !repo_branch.is_empty() {
        rows.push(stacked_row(
            &format!("{} {repo_branch}", muted("\u{e0a0}")),
            inner_w,
        ));
    }
    let url_avail = inner_w.saturating_sub(2 + ROUTES_LABEL_W);
    for (i, host) in hosts.iter().enumerate() {
        let url = if port == 443 {
            format!("https://{host}")
        } else {
            format!("https://{host}:{port}")
        };
        let url_t = truncate_str(&url, url_avail, "…");
        let line = if i == 0 {
            format!("{}  {url_color}{url_t}{RESET}", muted("routes"))
        } else {
            format!("{}{url_color}{url_t}{RESET}", " ".repeat(ROUTES_LABEL_W))
        };
        rows.push(stacked_row(&line, inner_w));
    }

    let cpu_str = if let Some(c) = cpu {
        format!("{} {:.0}%", muted("cpu"), c)
    } else {
        format!("{} —", muted("cpu"))
    };
    let ram_str = if let Some(m) = mem_bytes {
        format!("{} {}", muted("ram"), fmt_bytes(m))
    } else {
        format!("{} —", muted("ram"))
    };
    rows.push(stacked_row(&format!("{cpu_str}  {ram_str}"), inner_w));

    rows.push(bot);
    rows.join("\n")
}

fn stacked_row(content: &str, inner_w: usize) -> String {
    let content_area = inner_w.saturating_sub(2);
    let pad = content_area.saturating_sub(vlen(content));
    format!(
        "{BORDER}│{RESET} {content}{} {BORDER}│{RESET}",
        " ".repeat(pad)
    )
}

fn panel_row(c1: &str, c2: &str, c3: &str, col1_w: usize, col2_w: usize) -> String {
    let p1 = measure_text_width(c1);
    let p2 = measure_text_width(c2);
    let p3 = measure_text_width(c3);
    format!(
        "{BORDER}│{RESET} {c1}{}{c2}{}{c3}{} {BORDER}│{RESET}",
        " ".repeat(col1_w.saturating_sub(p1) + COL_SEP),
        " ".repeat(col2_w.saturating_sub(p2) + COL_SEP),
        " ".repeat(COL3_W.saturating_sub(p3)),
    )
}

pub(in crate::commands::dev) fn format_keymap() -> String {
    let cols = terminal_cols().max(20);
    let text = if cols < 60 {
        format!(
            "t {}   l {}   r {}   b {}   ^c/q {}",
            muted("tunnel"),
            muted("lan"),
            muted("restart"),
            muted("background"),
            muted("stop")
        )
    } else {
        format!(
            "t {}   l {}   r {}   b {}   ctrl+c/q {}",
            muted("tunnel"),
            muted("lan"),
            muted("restart"),
            muted("background"),
            muted("stop")
        )
    };
    let plain = if cols < 60 {
        "t tunnel   l lan   r restart   b background   ^c/q stop"
    } else {
        "t tunnel   l lan   r restart   b background   ctrl+c/q stop"
    };
    let pad = cols.saturating_sub(measure_text_width(plain) + 1);
    format!("{}{text} ", " ".repeat(pad))
}
