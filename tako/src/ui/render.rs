use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::output;

use super::{ELAPSED_GAP, TASK_INDENT, TaskIcon, TaskItemState, TaskState, TreeNode, TreeTextTone};

const COLOR_ACCENT: Color = Color::Rgb(125, 196, 228);
const COLOR_ERROR: Color = Color::Rgb(232, 163, 160);
const PROGRESS_BAR_WIDTH: usize = 16;
const BOX_SPINNER_TICKS: [&str; 4] = ["◧", "◨", "◩", "◪"];

pub(super) fn rendered_height(lines: &[Line<'_>], width: u16) -> u16 {
    let width = width.max(1) as usize;
    lines.iter().fold(0u16, |total, line| {
        let line_width = line.width();
        let rows = if line_width == 0 {
            1
        } else {
            line_width.div_ceil(width) as u16
        };
        total.saturating_add(rows)
    })
}

pub(super) fn tree_node_has_running(node: &TreeNode) -> bool {
    match node {
        TreeNode::Task(task) | TreeNode::AccentTask(task) => {
            matches!(task.state, TaskState::Running { .. })
                || task.children.iter().any(task_item_has_running)
        }
        TreeNode::Text { .. } | TreeNode::LabeledText { .. } => false,
        TreeNode::Spacer => false,
    }
}

fn task_item_has_running(task: &TaskItemState) -> bool {
    matches!(task.state, TaskState::Running { .. })
        || task.children.iter().any(task_item_has_running)
}

pub(super) fn render_tree_to_lines(tree: &[TreeNode], frame_index: usize) -> Vec<Line<'static>> {
    let now = Instant::now();
    let mut lines = Vec::new();
    for node in tree {
        match node {
            TreeNode::Task(task) => {
                render_task_item(
                    &mut lines,
                    task,
                    "",
                    RenderTaskOptions::new(now, frame_index),
                );
            }
            TreeNode::AccentTask(task) => {
                render_task_item(
                    &mut lines,
                    task,
                    "",
                    RenderTaskOptions::new(now, frame_index).accent(),
                );
            }
            TreeNode::Text { text, tone } => {
                let style = match tone {
                    TreeTextTone::Error => Style::new().fg(COLOR_ERROR),
                };
                lines.push(Line::from(vec![Span::styled(text.clone(), style)]));
            }
            TreeNode::LabeledText { label, value } => {
                let mut spans = Vec::new();
                if !label.is_empty() {
                    spans.push(Span::styled(
                        format!("{} ", label),
                        Style::new().fg(COLOR_ACCENT),
                    ));
                }
                spans.push(Span::raw(value.clone()));
                lines.push(Line::from(spans));
            }
            TreeNode::Spacer => {
                lines.push(Line::raw(""));
            }
        }
    }
    lines
}

#[derive(Clone, Copy)]
struct RenderTaskOptions {
    accent: bool,
    now: Instant,
    frame_index: usize,
}

impl RenderTaskOptions {
    fn new(now: Instant, frame_index: usize) -> Self {
        Self {
            accent: false,
            now,
            frame_index,
        }
    }

    fn accent(mut self) -> Self {
        self.accent = true;
        self
    }

    fn child(self) -> Self {
        Self {
            accent: false,
            ..self
        }
    }
}

fn render_task_item(
    lines: &mut Vec<Line<'static>>,
    task: &TaskItemState,
    prefix: &str,
    options: RenderTaskOptions,
) {
    let is_group = !task.children.is_empty();

    let icon = if is_group {
        ""
    } else {
        task_icon(task.icon, &task.state, options.frame_index)
    };
    let label = pending_task_label(&task.label, &task.state);
    let detail = format_task_detail(task);
    let elapsed = if is_group {
        None
    } else {
        format_task_elapsed(task, options.now)
    };

    let (icon_style, label_style, detail_style) =
        task_line_styles(&task.state, is_group || options.accent);

    let has_progress = task.progress.is_some();

    let mut spans = if icon.is_empty() {
        vec![Span::styled(format!("{prefix}{label}"), label_style)]
    } else {
        vec![
            Span::styled(format!("{prefix}{icon}"), icon_style),
            Span::styled(format!(" {label}"), label_style),
        ]
    };
    if !has_progress && let Some(detail) = detail.as_deref() {
        spans.push(Span::styled(format!(" {detail}"), detail_style));
    }
    if let Some(elapsed) = elapsed {
        spans.push(Span::raw(" ".repeat(ELAPSED_GAP)));
        spans.push(Span::styled(elapsed, detail_style));
    }
    if let Some(fraction) = task.progress {
        spans.push(Span::raw("  "));
        render_block_bar_spans(&mut spans, fraction);
        if let Some(detail) = detail.as_deref() {
            spans.push(Span::styled(format!("  {detail}"), detail_style));
        }
    }
    lines.push(Line::from(spans));

    if matches!(task.state, TaskState::Failed { .. })
        && let Some(detail) = task
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
    {
        lines.push(Line::from(vec![Span::styled(
            format!("{prefix}{TASK_INDENT}{detail}"),
            Style::new().fg(COLOR_ERROR),
        )]));
    }

    if is_group {
        let child_prefix = format!("{prefix}{TASK_INDENT}");
        for child in &task.children {
            render_task_item(lines, child, &child_prefix, options.child());
        }
    }
}

fn task_icon(icon: TaskIcon, state: &TaskState, frame_index: usize) -> &'static str {
    match icon {
        TaskIcon::Box => box_task_icon(state, frame_index),
        TaskIcon::State => state_task_icon(state, frame_index),
    }
}

fn box_task_icon(state: &TaskState, frame_index: usize) -> &'static str {
    match state {
        TaskState::Pending => "□",
        TaskState::Running { .. } => BOX_SPINNER_TICKS[frame_index % BOX_SPINNER_TICKS.len()],
        TaskState::Succeeded { .. } | TaskState::Failed { .. } => "■",
        TaskState::Skipped { .. } | TaskState::Cancelled { .. } => "□",
    }
}

fn state_task_icon(state: &TaskState, frame_index: usize) -> &'static str {
    match state {
        TaskState::Pending => "○",
        TaskState::Running { .. } => {
            output::SPINNER_TICKS[frame_index % output::SPINNER_TICKS.len()]
        }
        TaskState::Succeeded { .. } => "✔",
        TaskState::Failed { .. } => "✘",
        TaskState::Skipped { .. } | TaskState::Cancelled { .. } => "○",
    }
}

fn pending_task_label(label: &str, state: &TaskState) -> String {
    match state {
        TaskState::Pending => format!("{label}…"),
        TaskState::Running { .. } | TaskState::Skipped { .. } | TaskState::Cancelled { .. } => {
            format!("{label}…")
        }
        _ => label.to_string(),
    }
}

fn format_task_detail(task: &TaskItemState) -> Option<String> {
    if matches!(task.state, TaskState::Failed { .. }) {
        return None;
    }
    task.detail
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
}

fn format_task_elapsed(task: &TaskItemState, now: Instant) -> Option<String> {
    match &task.state {
        TaskState::Pending => None,
        TaskState::Running { started_at } => {
            let value = output::format_elapsed(now.saturating_duration_since(*started_at));
            (!value.is_empty()).then_some(value)
        }
        TaskState::Succeeded { elapsed }
        | TaskState::Failed { elapsed }
        | TaskState::Skipped { elapsed }
        | TaskState::Cancelled { elapsed } => elapsed.and_then(|e| {
            let value = output::format_elapsed_always(e);
            (!value.is_empty()).then_some(value)
        }),
    }
}

fn task_line_styles(state: &TaskState, is_group_like: bool) -> (Style, Style, Style) {
    let muted = Style::new().add_modifier(Modifier::DIM);
    let accent = Style::new().fg(COLOR_ACCENT);
    let error = Style::new().fg(COLOR_ERROR);
    let normal = Style::new();

    match state {
        TaskState::Pending => (muted, muted, muted),
        TaskState::Failed { .. } => (error, if is_group_like { accent } else { normal }, muted),
        TaskState::Skipped { .. } | TaskState::Cancelled { .. } => (muted, muted, muted),
        TaskState::Succeeded { .. } if is_group_like => (normal, accent, muted),
        TaskState::Succeeded { .. } => (normal, normal, muted),
        TaskState::Running { .. } if is_group_like => (accent, accent, muted),
        TaskState::Running { .. } => (normal, normal, muted),
    }
}

fn render_block_bar_spans(spans: &mut Vec<Span<'static>>, fraction: f64) {
    let f = fraction.clamp(0.0, 1.0);
    let filled = (f * PROGRESS_BAR_WIDTH as f64).round() as usize;
    let empty = PROGRESS_BAR_WIDTH.saturating_sub(filled);

    const LEFT: (u8, u8, u8) = (110, 170, 220);
    const RIGHT: (u8, u8, u8) = (155, 217, 179);

    for i in 0..filled {
        let t = if PROGRESS_BAR_WIDTH <= 1 {
            0.0
        } else {
            i as f64 / (PROGRESS_BAR_WIDTH - 1) as f64
        };
        let r = (LEFT.0 as f64 + (RIGHT.0 as f64 - LEFT.0 as f64) * t) as u8;
        let g = (LEFT.1 as f64 + (RIGHT.1 as f64 - LEFT.1 as f64) * t) as u8;
        let b = (LEFT.2 as f64 + (RIGHT.2 as f64 - LEFT.2 as f64) * t) as u8;
        spans.push(Span::styled("█", Style::new().fg(Color::Rgb(r, g, b))));
    }
    if empty > 0 {
        spans.push(Span::styled(
            "░".repeat(empty),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
}

#[allow(dead_code)] // used by deploy/upgrade test assertions
pub fn render_plain_lines(tree: &[TreeNode]) -> Vec<String> {
    let now = Instant::now();
    let mut lines = Vec::new();
    for node in tree {
        match node {
            TreeNode::Task(task) | TreeNode::AccentTask(task) => {
                render_task_item_plain(&mut lines, task, "", now);
            }
            TreeNode::Text { text, .. } => {
                lines.push(text.clone());
            }
            TreeNode::LabeledText { label, value } => {
                if label.is_empty() {
                    lines.push(value.clone());
                } else {
                    lines.push(format!("{label} {value}"));
                }
            }
            TreeNode::Spacer => {
                lines.push(String::new());
            }
        }
    }
    lines
}

fn render_task_item_plain(
    lines: &mut Vec<String>,
    task: &TaskItemState,
    prefix: &str,
    now: Instant,
) {
    let is_group = !task.children.is_empty();
    let icon = if is_group {
        ""
    } else {
        task_icon(task.icon, &task.state, 0)
    };
    let label = pending_task_label(&task.label, &task.state);
    let detail = format_task_detail(task);
    let elapsed = if is_group {
        None
    } else {
        format_task_elapsed(task, now)
    };

    let has_progress = task.progress.is_some();
    let mut line = if icon.is_empty() {
        format!("{prefix}{label}")
    } else {
        format!("{prefix}{icon} {label}")
    };
    if !has_progress && let Some(detail) = detail.as_deref() {
        line.push(' ');
        line.push_str(detail);
    }
    if let Some(elapsed) = elapsed {
        line.push_str(&" ".repeat(ELAPSED_GAP));
        line.push_str(&elapsed);
    }
    if has_progress {
        line.push_str(&" ".repeat(2 + PROGRESS_BAR_WIDTH));
        if let Some(detail) = detail.as_deref() {
            line.push_str("  ");
            line.push_str(detail);
        }
    }
    lines.push(line);

    if matches!(task.state, TaskState::Failed { .. })
        && let Some(detail) = task
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
    {
        lines.push(format!("{prefix}{TASK_INDENT}{detail}"));
    }

    if is_group {
        let child_prefix = format!("{prefix}{TASK_INDENT}");
        for child in &task.children {
            render_task_item_plain(lines, child, &child_prefix, now);
        }
    }
}
