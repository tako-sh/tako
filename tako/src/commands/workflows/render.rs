//! Human and JSON renderings of workflow runs.
//!
//! Renderers return plain lines so they can be tested without a terminal;
//! [`emit`] writes them through the shared output helpers.

use serde_json::{Value, json};
use tako_core::{
    CappedJson, ListWorkflowRunsResponse, WorkflowRunDetail, WorkflowRunStatus,
    WorkflowRunTransition,
};
use time::UtcOffset;
use unicode_width::UnicodeWidthStr;

use super::{Answer, ServerError};
use crate::output;

const ERROR_COLUMN_MAX_CHARS: usize = 60;
const STEP_RESULT_MAX_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Line {
    Section(String),
    Text(String),
    Muted(String),
}

pub(super) fn emit(lines: &[Line]) {
    for line in lines {
        match line {
            Line::Section(title) => output::section(title),
            Line::Text(text) if output::is_pretty() => output::info(text),
            Line::Muted(text) if output::is_pretty() => output::muted(text),
            Line::Text(text) | Line::Muted(text) => tracing::info!("{text}"),
        }
    }
}

/// The current time and local offset used for timestamps.
#[derive(Debug, Clone, Copy)]
pub(super) struct Clock {
    pub now_ms: i64,
    pub offset: UtcOffset,
}

impl Clock {
    pub fn local() -> Self {
        Self {
            now_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            offset: crate::commands::releases::local_offset(),
        }
    }

    fn absolute(&self, ms: i64) -> String {
        crate::commands::releases::format_unix_timestamp_with_offset(
            ms.div_euclid(1000),
            self.offset,
        )
        .unwrap_or_else(|| "-".to_string())
    }

    /// `5m ago`, `in 3h`, or `None` beyond a day.
    fn relative(&self, ms: i64) -> Option<String> {
        let delta = (self.now_ms - ms) / 1000;
        let magnitude = delta.unsigned_abs();
        if magnitude > 86_400 {
            return None;
        }
        if magnitude < 60 {
            return Some(if delta >= 0 { "just now" } else { "now" }.to_string());
        }
        let amount = if magnitude < 3_600 {
            format!("{}m", magnitude / 60)
        } else {
            format!("{}h", magnitude / 3_600)
        };
        Some(if delta >= 0 {
            format!("{amount} ago")
        } else {
            format!("in {amount}")
        })
    }

    /// Relative within a day, absolute otherwise.
    fn short(&self, ms: i64) -> String {
        self.relative(ms).unwrap_or_else(|| self.absolute(ms))
    }

    /// Absolute, with the relative time within a day.
    fn full(&self, ms: i64) -> String {
        match self.relative(ms) {
            Some(relative) => format!("{}, {relative}", self.absolute(ms)),
            None => self.absolute(ms),
        }
    }
}

pub(super) struct Heading<'a> {
    pub app: &'a str,
    pub env: &'a str,
}

impl Heading<'_> {
    fn line(&self) -> String {
        format!("{} ({})", self.app, self.env)
    }
}

pub(super) fn list_lines(
    heading: &Heading<'_>,
    list: &ListWorkflowRunsResponse,
    clock: &Clock,
) -> Vec<Line> {
    let mut lines = vec![
        Line::Section("Workflow runs".to_string()),
        Line::Text(heading.line()),
    ];
    if list.runs.is_empty() {
        lines.push(Line::Muted("No workflow runs found.".to_string()));
        return lines;
    }

    let header = ["ID", "WORKFLOW", "STATUS", "ATTEMPTS", "WHEN", "ERROR"];
    let rows: Vec<Vec<String>> = list
        .runs
        .iter()
        .map(|run| {
            vec![
                run.id.clone(),
                run.name.clone(),
                run.status.to_string(),
                format!("{}/{}", run.attempts, run.max_attempts),
                format_when(run.status, run.run_at_ms, run.finished_at_ms, clock),
                run.last_error
                    .as_deref()
                    .map(|error| single_line(error, ERROR_COLUMN_MAX_CHARS))
                    .unwrap_or_default(),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|column| {
            rows.iter()
                .map(|cells| cells[column].width())
                .chain(std::iter::once(header[column].width()))
                .max()
                .unwrap_or_default()
        })
        .collect();
    let header: Vec<String> = header.into_iter().map(str::to_string).collect();
    lines.push(Line::Text(table_row(&header, &widths)));
    lines.extend(
        rows.iter()
            .map(|cells| Line::Text(table_row(cells, &widths))),
    );

    if list.has_more {
        lines.push(Line::Muted(format!(
            "Showing the newest {} runs. Narrow with --status or --name, or raise --limit (up to {}).",
            list.runs.len(),
            tako_core::WORKFLOW_RUNS_MAX_LIMIT
        )));
    }
    lines
}

fn table_row(cells: &[String], widths: &[usize]) -> String {
    let last = cells.len() - 1;
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            if index == last {
                cell.clone()
            } else {
                format!(
                    "{cell}{}",
                    " ".repeat(widths[index].saturating_sub(cell.width()))
                )
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

/// When a run is next due, or when it finished.
fn format_when(
    status: WorkflowRunStatus,
    run_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
    clock: &Clock,
) -> String {
    match status {
        WorkflowRunStatus::Pending => match run_at_ms {
            Some(at) if at <= clock.now_ms => "due now".to_string(),
            Some(at) => format!("next {}", clock.short(at)),
            None => "waiting for signal".to_string(),
        },
        WorkflowRunStatus::Running => "running".to_string(),
        WorkflowRunStatus::Succeeded | WorkflowRunStatus::Cancelled | WorkflowRunStatus::Dead => {
            finished_at_ms
                .map(|at| format!("finished {}", clock.short(at)))
                .unwrap_or_else(|| "-".to_string())
        }
    }
}

/// First line of `text`, collapsed and cut to `max_chars`.
fn single_line(text: &str, max_chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&line, max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

pub(super) fn show_lines(
    heading: &Heading<'_>,
    run: &WorkflowRunDetail,
    clock: &Clock,
) -> Vec<Line> {
    let field = |label: &str, value: String| Line::Text(format!("{label:<10}{value}"));
    let mut lines = vec![
        Line::Section("Workflow run".to_string()),
        Line::Text(heading.line()),
        field("ID", run.id.clone()),
        field("Workflow", run.name.clone()),
        field("Status", run.status.to_string()),
        field("Attempts", format!("{}/{}", run.attempts, run.max_attempts)),
        field("Created", clock.full(run.created_at_ms)),
    ];
    match run.status {
        WorkflowRunStatus::Pending => lines.push(field(
            "Next run",
            run.run_at_ms
                .map(|at| clock.full(at))
                .unwrap_or_else(|| "waiting for signal".to_string()),
        )),
        WorkflowRunStatus::Running => {
            if let Some(lease) = run.lease_until_ms {
                lines.push(field("Lease", format!("until {}", clock.full(lease))));
            }
            if let Some(worker) = &run.worker_id {
                lines.push(field("Worker", worker.clone()));
            }
        }
        WorkflowRunStatus::Succeeded | WorkflowRunStatus::Cancelled | WorkflowRunStatus::Dead => {
            if let Some(finished) = run.finished_at_ms {
                lines.push(field("Finished", clock.full(finished)));
            }
        }
    }

    if let Some(error) = &run.last_error {
        lines.push(Line::Section("Error".to_string()));
        lines.extend(error.lines().map(|line| Line::Text(line.to_string())));
        if run.last_error_truncated {
            lines.push(Line::Muted("Error truncated".to_string()));
        }
    }

    lines.push(Line::Section("Payload".to_string()));
    lines.extend(capped_lines(&run.payload));
    if let Some(result) = &run.result {
        lines.push(Line::Section("Result".to_string()));
        lines.extend(capped_lines(result));
    }

    lines.push(Line::Section("Steps".to_string()));
    let omitted = run.steps_total.saturating_sub(run.steps.len() as u64);
    if omitted > 0 {
        lines.push(Line::Muted(format!(
            "{omitted} older step(s) omitted; showing the {} most recent of {}.",
            run.steps.len(),
            run.steps_total
        )));
    }
    if run.steps.is_empty() {
        lines.push(Line::Muted("No saved steps.".to_string()));
    }
    let name_width = run
        .steps
        .iter()
        .map(|step| step.name.width())
        .max()
        .unwrap_or_default();
    for step in &run.steps {
        lines.push(Line::Text(format!(
            "{}{}  {}  {}",
            step.name,
            " ".repeat(name_width.saturating_sub(step.name.width())),
            clock.absolute(step.completed_at_ms),
            compact(&step.result)
        )));
    }

    if !run.waits.is_empty() {
        lines.push(Line::Section("Waits".to_string()));
        for wait in &run.waits {
            let until = wait
                .expires_at_ms
                .map(|at| format!("until {}", clock.full(at)))
                .unwrap_or_else(|| "with no timeout".to_string());
            // Waits on another run's outcome park on the reserved `__run:<id>` event.
            let target = match wait.event_name.strip_prefix("__run:") {
                Some(run) => format!("run {}", printable(run)),
                None => printable(&wait.event_name),
            };
            lines.push(Line::Text(format!(
                "{} waits for {target} {until}",
                printable(&wait.step_name)
            )));
        }
    }

    if run.signals_total > 0 {
        lines.push(Line::Section("Buffered signals".to_string()));
        let omitted = run.signals_total.saturating_sub(run.signals.len() as u64);
        if omitted > 0 {
            lines.push(Line::Muted(format!(
                "{omitted} newer signal(s) omitted; showing the {} oldest of {}.",
                run.signals.len(),
                run.signals_total
            )));
        }
        // Senders choose these names; never let one drive the terminal.
        let names: Vec<String> = run
            .signals
            .iter()
            .map(|signal| printable(&signal.event_name))
            .collect();
        let name_width = names
            .iter()
            .map(|name| name.width())
            .max()
            .unwrap_or_default();
        for (signal, name) in run.signals.iter().zip(&names) {
            lines.push(Line::Text(format!(
                "{}{}  {}  {} bytes",
                name,
                " ".repeat(name_width.saturating_sub(name.width())),
                clock.absolute(signal.received_at_ms),
                signal.bytes
            )));
        }
    }
    lines
}

/// `text` with control characters (ANSI escapes, newlines) shown as escapes,
/// so stored names cannot move the cursor or forge lines.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().collect()
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn capped_lines(value: &CappedJson) -> Vec<Line> {
    if value.truncated {
        let mut lines: Vec<Line> = value
            .preview
            .as_deref()
            .unwrap_or_default()
            .lines()
            .map(|line| Line::Text(line.to_string()))
            .collect();
        lines.push(Line::Muted(format!("truncated, {} bytes", value.bytes)));
        return lines;
    }
    serde_json::to_string_pretty(&value.value)
        .unwrap_or_else(|_| value.value.to_string())
        .lines()
        .map(|line| Line::Text(line.to_string()))
        .collect()
}

/// One-line rendering of a step result.
fn compact(value: &CappedJson) -> String {
    if value.truncated {
        let preview = single_line(
            value.preview.as_deref().unwrap_or_default(),
            STEP_RESULT_MAX_CHARS,
        );
        return format!("{preview} (truncated, {} bytes)", value.bytes);
    }
    truncate_chars(&value.value.to_string(), STEP_RESULT_MAX_CHARS)
}

pub(super) fn transition_line(action: &str, transition: &WorkflowRunTransition) -> String {
    format!(
        "{action} {}: {} -> {}",
        transition.id, transition.previous_status, transition.status
    )
}

pub(super) fn list_json(
    app: &str,
    env: &str,
    list: &ListWorkflowRunsResponse,
    errors: &[ServerError],
) -> Value {
    json!({
        "ok": true,
        "command": "workflow",
        "app": app,
        "environment": env,
        "runs": list.runs,
        "has_more": list.has_more,
        "errors": errors,
    })
}

pub(super) fn show_json(app: &str, env: &str, found: &Answer<WorkflowRunDetail>) -> Value {
    json!({
        "ok": true,
        "command": "workflow",
        "app": app,
        "environment": env,
        "run": found.reply,
        "errors": found.errors,
    })
}

pub(super) fn transition_json(
    app: &str,
    env: &str,
    transition: &WorkflowRunTransition,
    errors: &[ServerError],
) -> Value {
    json!({
        "ok": true,
        "command": "workflow",
        "app": app,
        "environment": env,
        "id": transition.id,
        "previous_status": transition.previous_status,
        "status": transition.status,
        "errors": errors,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use tako_core::{WorkflowRunSignal, WorkflowRunStep, WorkflowRunSummary, WorkflowRunWait};

    // 2026-11-12 12:00:00 UTC
    const NOW_MS: i64 = 1_794_484_800_000;
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;

    fn clock() -> Clock {
        Clock {
            now_ms: NOW_MS,
            offset: UtcOffset::UTC,
        }
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| match line {
                Line::Section(text) => format!("# {text}"),
                Line::Text(text) => text.clone(),
                Line::Muted(text) => format!("~ {text}"),
            })
            .collect()
    }

    fn summary(id: &str, created_at_ms: i64) -> WorkflowRunSummary {
        WorkflowRunSummary {
            id: id.to_string(),
            name: "send-email".to_string(),
            status: WorkflowRunStatus::Dead,
            attempts: 3,
            max_attempts: 3,
            created_at_ms,
            run_at_ms: Some(created_at_ms),
            finished_at_ms: Some(created_at_ms + 1_000),
            last_error: Some("SMTP timeout".to_string()),
            last_error_truncated: false,
        }
    }

    pub(crate) fn detail(id: &str) -> WorkflowRunDetail {
        WorkflowRunDetail {
            id: id.to_string(),
            name: "send-email".to_string(),
            status: WorkflowRunStatus::Dead,
            attempts: 3,
            max_attempts: 3,
            created_at_ms: 0,
            run_at_ms: Some(0),
            finished_at_ms: Some(1_000),
            lease_until_ms: None,
            worker_id: None,
            last_error: Some("SMTP timeout".to_string()),
            last_error_truncated: false,
            payload: CappedJson {
                value: json!({"to": "a@example.com"}),
                truncated: false,
                bytes: 22,
                preview: None,
            },
            result: None,
            steps: Vec::new(),
            steps_total: 0,
            waits: Vec::new(),
            signals: Vec::new(),
            signals_total: 0,
        }
    }

    fn heading() -> Heading<'static> {
        Heading {
            app: "demo",
            env: "production",
        }
    }

    fn list(runs: Vec<WorkflowRunSummary>, has_more: bool) -> ListWorkflowRunsResponse {
        ListWorkflowRunsResponse {
            app: "demo/production".to_string(),
            runs,
            has_more,
        }
    }

    fn sample_rows() -> Vec<WorkflowRunSummary> {
        let mut dead = summary("run_dead", NOW_MS - 3 * HOUR);
        dead.finished_at_ms = Some(NOW_MS - 2 * HOUR);
        dead.last_error = Some("SMTP timeout\n    at send (mail.ts:10)".into());
        let mut pending = summary("run_next", NOW_MS - 10 * MINUTE);
        pending.status = WorkflowRunStatus::Pending;
        pending.attempts = 1;
        pending.max_attempts = 5;
        pending.run_at_ms = Some(NOW_MS + 5 * MINUTE);
        pending.finished_at_ms = None;
        pending.last_error = None;
        let mut running = summary("run_live", NOW_MS - MINUTE);
        running.status = WorkflowRunStatus::Running;
        running.attempts = 1;
        running.finished_at_ms = None;
        running.last_error = None;
        vec![running, pending, dead]
    }

    #[test]
    fn list_renders_a_table() {
        let lines = list_lines(&heading(), &list(sample_rows(), false), &clock());
        assert_eq!(
            texts(&lines),
            [
                "# Workflow runs",
                "demo (production)",
                "ID        WORKFLOW    STATUS   ATTEMPTS  WHEN             ERROR",
                "run_live  send-email  running  1/3       running",
                "run_next  send-email  pending  1/5       next in 5m",
                "run_dead  send-email  dead     3/3       finished 2h ago  SMTP timeout at send (mail.ts:10)",
            ]
        );
    }

    #[test]
    fn list_with_more_runs_hint() {
        let lines = list_lines(
            &heading(),
            &list(sample_rows()[2..].to_vec(), true),
            &clock(),
        );
        assert_eq!(
            texts(&lines),
            [
                "# Workflow runs",
                "demo (production)",
                "ID        WORKFLOW    STATUS  ATTEMPTS  WHEN             ERROR",
                "run_dead  send-email  dead    3/3       finished 2h ago  SMTP timeout at send (mail.ts:10)",
                "~ Showing the newest 1 runs. Narrow with --status or --name, or raise --limit (up to 200).",
            ]
        );
    }

    #[test]
    fn list_empty_state() {
        let lines = list_lines(&heading(), &list(Vec::new(), false), &clock());
        assert_eq!(
            texts(&lines),
            [
                "# Workflow runs",
                "demo (production)",
                "~ No workflow runs found."
            ]
        );
    }

    #[test]
    fn when_column_covers_every_status() {
        let clock = clock();
        let pending = WorkflowRunStatus::Pending;
        assert_eq!(
            format_when(pending, Some(NOW_MS - 1), None, &clock),
            "due now"
        );
        assert_eq!(
            format_when(pending, None, None, &clock),
            "waiting for signal"
        );
        assert_eq!(
            format_when(pending, Some(NOW_MS + 3 * 86_400_000), None, &clock),
            "next 2026-11-15 12:00:00"
        );
        assert_eq!(
            format_when(
                WorkflowRunStatus::Succeeded,
                None,
                Some(NOW_MS - 30_000),
                &clock
            ),
            "finished just now"
        );
    }

    #[test]
    fn errors_are_cut_to_one_short_line() {
        let long = "x".repeat(100);
        let cut = single_line(&long, 60);
        assert_eq!(cut.chars().count(), 60);
        assert!(cut.ends_with('…'));
        assert_eq!(single_line("a\n  b\tc", 60), "a b c");
    }

    fn sample_detail() -> WorkflowRunDetail {
        let mut run = detail("run_dead");
        run.created_at_ms = NOW_MS - 3 * HOUR;
        run.finished_at_ms = Some(NOW_MS - 2 * HOUR);
        run.last_error = Some("SMTP timeout\n    at send (mail.ts:10)".into());
        run.steps = vec![
            WorkflowRunStep {
                name: "render".into(),
                completed_at_ms: NOW_MS - 2 * HOUR - 2 * MINUTE,
                result: CappedJson {
                    value: json!({"subject": "Welcome"}),
                    truncated: false,
                    bytes: 21,
                    preview: None,
                },
            },
            WorkflowRunStep {
                name: "attachments".into(),
                completed_at_ms: NOW_MS - 2 * HOUR - MINUTE,
                result: CappedJson {
                    value: Value::Null,
                    truncated: true,
                    bytes: 4096,
                    preview: Some("[\"a.pdf\",\"b.pdf\"".into()),
                },
            },
        ];
        run.steps_total = 3;
        run
    }

    #[test]
    fn show_renders_fields_payload_steps_and_error() {
        let lines = show_lines(&heading(), &sample_detail(), &clock());
        assert_eq!(
            texts(&lines),
            [
                "# Workflow run",
                "demo (production)",
                "ID        run_dead",
                "Workflow  send-email",
                "Status    dead",
                "Attempts  3/3",
                "Created   2026-11-12 09:00:00, 3h ago",
                "Finished  2026-11-12 10:00:00, 2h ago",
                "# Error",
                "SMTP timeout",
                "    at send (mail.ts:10)",
                "# Payload",
                "{",
                "  \"to\": \"a@example.com\"",
                "}",
                "# Steps",
                "~ 1 older step(s) omitted; showing the 2 most recent of 3.",
                "render       2026-11-12 09:58:00  {\"subject\":\"Welcome\"}",
                "attachments  2026-11-12 09:59:00  [\"a.pdf\",\"b.pdf\" (truncated, 4096 bytes)",
            ]
        );
    }

    #[test]
    fn show_renders_running_lease_truncated_values_and_waits() {
        let mut run = detail("run_live");
        run.status = WorkflowRunStatus::Running;
        run.created_at_ms = NOW_MS - 5 * MINUTE;
        run.finished_at_ms = None;
        run.last_error = None;
        run.lease_until_ms = Some(NOW_MS + MINUTE);
        run.worker_id = Some("worker-7".into());
        run.payload = CappedJson {
            value: Value::Null,
            truncated: true,
            bytes: 40_000,
            preview: Some("{\"rows\":[1,2,3".into()),
        };
        run.waits = vec![
            WorkflowRunWait {
                step_name: "approval".into(),
                event_name: "order.approved".into(),
                expires_at_ms: None,
            },
            WorkflowRunWait {
                step_name: "invoice:outcome".into(),
                event_name: "__run:child:42".into(),
                expires_at_ms: None,
            },
        ];
        run.signals = vec![
            WorkflowRunSignal {
                event_name: "shipped".into(),
                received_at_ms: NOW_MS - 2 * MINUTE,
                bytes: 17,
            },
            WorkflowRunSignal {
                event_name: "delivered\u{1b}[2J".into(),
                received_at_ms: NOW_MS - MINUTE,
                bytes: 4,
            },
        ];
        run.signals_total = 3;
        let lines = show_lines(&heading(), &run, &clock());
        assert_eq!(
            texts(&lines),
            [
                "# Workflow run",
                "demo (production)",
                "ID        run_live",
                "Workflow  send-email",
                "Status    running",
                "Attempts  3/3",
                "Created   2026-11-12 11:55:00, 5m ago",
                "Lease     until 2026-11-12 12:01:00, in 1m",
                "Worker    worker-7",
                "# Payload",
                "{\"rows\":[1,2,3",
                "~ truncated, 40000 bytes",
                "# Steps",
                "~ No saved steps.",
                "# Waits",
                "approval waits for order.approved with no timeout",
                "invoice:outcome waits for run child:42 with no timeout",
                "# Buffered signals",
                "~ 1 newer signal(s) omitted; showing the 2 oldest of 3.",
                "shipped             2026-11-12 11:58:00  17 bytes",
                "delivered\\u{1b}[2J  2026-11-12 11:59:00  4 bytes",
            ]
        );
    }

    fn transition(previous: WorkflowRunStatus, status: WorkflowRunStatus) -> WorkflowRunTransition {
        WorkflowRunTransition {
            app: "demo/production".into(),
            id: "run_dead".into(),
            previous_status: previous,
            status,
        }
    }

    #[test]
    fn transition_lines_state_the_change() {
        assert_eq!(
            transition_line(
                "Retried",
                &transition(WorkflowRunStatus::Dead, WorkflowRunStatus::Pending)
            ),
            "Retried run_dead: dead -> pending"
        );
        assert_eq!(
            transition_line(
                "Cancelled",
                &transition(WorkflowRunStatus::Running, WorkflowRunStatus::Cancelled)
            ),
            "Cancelled run_dead: running -> cancelled"
        );
    }

    #[test]
    fn json_shapes_are_stable() {
        let errors = [ServerError {
            name: "nyc".into(),
            error: "connection refused".into(),
        }];
        let value = list_json(
            "demo",
            "production",
            &list(sample_rows()[2..].to_vec(), false),
            &errors,
        );
        assert_eq!(
            value,
            json!({
                "ok": true,
                "command": "workflow",
                "app": "demo",
                "environment": "production",
                "runs": [{
                    "id": "run_dead",
                    "name": "send-email",
                    "status": "dead",
                    "attempts": 3,
                    "max_attempts": 3,
                    "created_at_ms": NOW_MS - 3 * HOUR,
                    "run_at_ms": NOW_MS - 3 * HOUR,
                    "finished_at_ms": NOW_MS - 2 * HOUR,
                    "last_error": "SMTP timeout\n    at send (mail.ts:10)",
                    "last_error_truncated": false,
                }],
                "has_more": false,
                "errors": [{"name": "nyc", "error": "connection refused"}],
            })
        );

        let found = Answer {
            server: "la".into(),
            reply: detail("run_dead"),
            errors: Vec::new(),
        };
        let value = show_json("demo", "production", &found);
        assert_eq!(
            value,
            json!({
                "ok": true,
                "command": "workflow",
                "app": "demo",
                "environment": "production",
                "run": detail("run_dead"),
                "errors": [],
            })
        );
        assert_eq!(value["run"]["payload"]["value"]["to"], "a@example.com");
        assert_eq!(value["run"]["steps_total"], 0);

        let value = transition_json(
            "demo",
            "production",
            &transition(WorkflowRunStatus::Pending, WorkflowRunStatus::Cancelled),
            &[],
        );
        assert_eq!(
            value,
            json!({
                "ok": true,
                "command": "workflow",
                "app": "demo",
                "environment": "production",
                "id": "run_dead",
                "previous_status": "pending",
                "status": "cancelled",
                "errors": [],
            })
        );
    }
}
