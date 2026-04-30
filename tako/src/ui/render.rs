use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::output;

use super::{TASK_INDENT, TIME_COL_GAP, TaskItemState, TaskState, TreeNode, TreeTextTone};

const COLOR_ACCENT: Color = Color::Rgb(125, 196, 228);
const COLOR_SUCCESS: Color = Color::Rgb(155, 217, 179);
const COLOR_ERROR: Color = Color::Rgb(232, 163, 160);
const PROGRESS_BAR_WIDTH: usize = 16;

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
    let time_col = compute_time_align_col(tree);
    let mut lines = Vec::new();
    for node in tree {
        match node {
            TreeNode::Task(task) => {
                render_task_item(
                    &mut lines,
                    task,
                    "",
                    RenderTaskOptions::new(time_col, now, frame_index),
                );
            }
            TreeNode::AccentTask(task) => {
                render_task_item(
                    &mut lines,
                    task,
                    "",
                    RenderTaskOptions::new(time_col, now, frame_index).accent(),
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
    hide_success_icon: bool,
    force_muted: bool,
    time_col: usize,
    now: Instant,
    frame_index: usize,
}

impl RenderTaskOptions {
    fn new(time_col: usize, now: Instant, frame_index: usize) -> Self {
        Self {
            accent: false,
            hide_success_icon: false,
            force_muted: false,
            time_col,
            now,
            frame_index,
        }
    }

    fn accent(mut self) -> Self {
        self.accent = true;
        self
    }

    fn child(self, parent_succeeded: bool) -> Self {
        Self {
            accent: false,
            hide_success_icon: parent_succeeded,
            force_muted: parent_succeeded,
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

    let icon = task_icon(&task.state, options.frame_index, options.hide_success_icon);
    let label = pending_task_label(&task.label, &task.state);
    let detail = format_task_detail(task);
    let elapsed = format_task_elapsed(task, options.now);

    let (icon_style, label_style, detail_style) = if options.force_muted {
        let m = Style::new().add_modifier(Modifier::DIM);
        (m, m, m)
    } else {
        task_line_styles(&task.state, is_group || options.accent)
    };

    let has_progress = task.progress.is_some();

    let mut spans = vec![
        Span::styled(format!("{prefix}{icon}"), icon_style),
        Span::styled(format!(" {label}"), label_style),
    ];
    let mut content_width =
        prefix.chars().count() + icon.chars().count() + 1 + label.chars().count();

    if !has_progress && let Some(detail) = detail.as_deref() {
        spans.push(Span::styled(format!(" {detail}"), detail_style));
        content_width += 1 + detail.chars().count();
    }
    if let Some(elapsed) = elapsed {
        let pad = options
            .time_col
            .saturating_sub(content_width)
            .max(TIME_COL_GAP);
        spans.push(Span::raw(" ".repeat(pad)));
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
        let parent_succeeded = matches!(task.state, TaskState::Succeeded { .. });
        let child_prefix = format!("{prefix}{TASK_INDENT}");
        for child in &task.children {
            render_task_item(lines, child, &child_prefix, options.child(parent_succeeded));
        }
    }
}

fn compute_time_align_col(tree: &[TreeNode]) -> usize {
    let mut max = 0usize;
    for node in tree {
        match node {
            TreeNode::Task(task) | TreeNode::AccentTask(task) => {
                visit_task_width(task, "", &mut max);
            }
            _ => {}
        }
    }
    max + TIME_COL_GAP
}

fn visit_task_width(task: &TaskItemState, prefix: &str, max: &mut usize) {
    let label = pending_task_label(&task.label, &task.state);
    let mut width = prefix.chars().count() + 1 + 1 + label.chars().count();
    if task.progress.is_none()
        && let Some(detail) = format_task_detail(task)
    {
        width += 1 + detail.chars().count();
    }
    if width > *max {
        *max = width;
    }
    let child_prefix = format!("{prefix}{TASK_INDENT}");
    for child in &task.children {
        visit_task_width(child, &child_prefix, max);
    }
}

fn task_icon(state: &TaskState, frame_index: usize, hide_success_icon: bool) -> &'static str {
    match state {
        TaskState::Succeeded { .. } if hide_success_icon => "·",
        TaskState::Pending => "○",
        TaskState::Running { .. } => {
            output::SPINNER_TICKS[frame_index % output::SPINNER_TICKS.len()]
        }
        TaskState::Succeeded { .. } => "✔",
        TaskState::Failed { .. } => "✘",
        TaskState::Skipped { .. } => "⏭",
        TaskState::Cancelled { .. } => "⊘",
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
    let success = Style::new().fg(COLOR_SUCCESS);
    let error = Style::new().fg(COLOR_ERROR);
    let normal = Style::new();

    match state {
        TaskState::Pending => (muted, muted, muted),
        TaskState::Failed { .. } => (error, if is_group_like { accent } else { normal }, muted),
        TaskState::Skipped { .. } | TaskState::Cancelled { .. } => (muted, muted, muted),
        TaskState::Succeeded { .. } => {
            (success, if is_group_like { accent } else { normal }, muted)
        }
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
    let time_col = compute_time_align_col(tree);
    let mut lines = Vec::new();
    for node in tree {
        match node {
            TreeNode::Task(task) | TreeNode::AccentTask(task) => {
                render_task_item_plain(&mut lines, task, "", false, time_col, now);
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
    hide_success_icon: bool,
    time_col: usize,
    now: Instant,
) {
    let is_group = !task.children.is_empty();
    let icon = task_icon(&task.state, 0, hide_success_icon);
    let label = pending_task_label(&task.label, &task.state);
    let detail = format_task_detail(task);
    let elapsed = format_task_elapsed(task, now);

    let has_progress = task.progress.is_some();
    let mut line = format!("{prefix}{icon} {label}");
    let mut content_width =
        prefix.chars().count() + icon.chars().count() + 1 + label.chars().count();
    if !has_progress && let Some(detail) = detail.as_deref() {
        line.push(' ');
        line.push_str(detail);
        content_width += 1 + detail.chars().count();
    }
    if let Some(elapsed) = elapsed {
        let pad = time_col.saturating_sub(content_width).max(TIME_COL_GAP);
        line.push_str(&" ".repeat(pad));
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
        let children_hide_success = matches!(task.state, TaskState::Succeeded { .. });
        let child_prefix = format!("{prefix}{TASK_INDENT}");
        for child in &task.children {
            render_task_item_plain(
                lines,
                child,
                &child_prefix,
                children_hide_success,
                time_col,
                now,
            );
        }
    }
}
