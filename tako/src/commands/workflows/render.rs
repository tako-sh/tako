//! Human and JSON renderings of workflow runs.
//!
//! Renderers return plain lines so they can be tested without a terminal;
//! [`emit`] writes them through the shared output helpers.

use serde_json::{Value, json};
use tako_core::{
    CappedJson, WorkflowRunDetail, WorkflowRunStatus, WorkflowRunTransition, WorkflowStoreKind,
};
use time::UtcOffset;
use unicode_width::UnicodeWidthStr;

use super::merge::{FoundRun, MergedRuns};
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
    /// Shown when the result belongs to one server of several.
    pub server: Option<&'a str>,
}

impl Heading<'_> {
    fn line(&self) -> String {
        match self.server {
            Some(server) => format!("{} ({}) on {server}", self.app, self.env),
            None => format!("{} ({})", self.app, self.env),
        }
    }
}

pub(super) fn list_lines(
    heading: &Heading<'_>,
    merged: &MergedRuns,
    show_server: bool,
    clock: &Clock,
) -> Vec<Line> {
    let mut lines = vec![
        Line::Section("Workflow runs".to_string()),
        Line::Text(heading.line()),
    ];
    if merged.runs.is_empty() {
        lines.push(Line::Muted("No workflow runs found.".to_string()));
        return lines;
    }

    let mut header = vec!["ID", "WORKFLOW", "STATUS", "ATTEMPTS"];
    if show_server {
        header.push("SERVER");
    }
    header.extend(["WHEN", "ERROR"]);
    let rows: Vec<Vec<String>> = merged
        .runs
        .iter()
        .map(|row| {
            let run = &row.run;
            let mut cells = vec![
                run.id.clone(),
                run.name.clone(),
                run.status.to_string(),
                format!("{}/{}", run.attempts, run.max_attempts),
            ];
            if show_server {
                cells.push(row.server.clone().unwrap_or_else(|| "-".to_string()));
            }
            cells.push(format_when(
                run.status,
                run.run_at_ms,
                run.finished_at_ms,
                clock,
            ));
            cells.push(
                run.last_error
                    .as_deref()
                    .map(|error| single_line(error, ERROR_COLUMN_MAX_CHARS))
                    .unwrap_or_default(),
            );
            cells
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

    if merged.has_more {
        lines.push(Line::Muted(format!(
            "Showing the newest {} runs. Narrow with --status or --name, or raise --limit (up to {}).",
            merged.runs.len(),
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
            lines.push(Line::Text(format!(
                "{} waits for {} {until}",
                wait.step_name, wait.event_name
            )));
        }
    }
    lines
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

pub(super) fn transition_line(
    action: &str,
    transition: &WorkflowRunTransition,
    server: Option<&str>,
) -> String {
    let location = server.map(|name| format!(" on {name}")).unwrap_or_default();
    format!(
        "{action} {}{location}: {} -> {}",
        transition.id, transition.previous_status, transition.status
    )
}

pub(super) fn errors_json(errors: &[super::merge::ServerError]) -> Value {
    json!(errors)
}

pub(super) fn list_json(app: &str, env: &str, merged: &MergedRuns) -> Value {
    json!({
        "ok": true,
        "command": "workflows",
        "app": app,
        "environment": env,
        "store": merged.store,
        "runs": merged.runs,
        "has_more": merged.has_more,
        "errors": errors_json(&merged.errors),
    })
}

pub(super) fn show_json(app: &str, env: &str, found: &FoundRun) -> Value {
    json!({
        "ok": true,
        "command": "workflows",
        "app": app,
        "environment": env,
        "store": found.store,
        "server": found.server_label(),
        "run": found.run,
        "errors": errors_json(&found.errors),
    })
}

pub(super) fn transition_json(
    app: &str,
    env: &str,
    found: &FoundRun,
    transition: &WorkflowRunTransition,
) -> Value {
    json!({
        "ok": true,
        "command": "workflows",
        "app": app,
        "environment": env,
        "store": transition.store,
        "server": (transition.store == WorkflowStoreKind::Sqlite).then_some(found.server.as_str()),
        "id": transition.id,
        "previous_status": transition.previous_status,
        "status": transition.status,
        "errors": errors_json(&found.errors),
    })
}

#[cfg(test)]
mod tests {
    use super::super::merge::tests::{detail, summary};
    use super::super::merge::{ListedRun, ServerError};
    use super::*;
    use tako_core::{WorkflowRunStep, WorkflowRunWait};

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

    fn heading(server: Option<&str>) -> Heading<'_> {
        Heading {
            app: "demo",
            env: "production",
            server,
        }
    }

    fn merged(runs: Vec<ListedRun>, has_more: bool) -> MergedRuns {
        MergedRuns {
            store: WorkflowStoreKind::Sqlite,
            runs,
            has_more,
            errors: Vec::new(),
        }
    }

    fn sample_rows() -> Vec<ListedRun> {
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
        vec![
            ListedRun {
                run: running,
                server: Some("nyc".into()),
            },
            ListedRun {
                run: pending,
                server: Some("la".into()),
            },
            ListedRun {
                run: dead,
                server: Some("la".into()),
            },
        ]
    }

    #[test]
    fn list_renders_a_table_with_server_column() {
        let lines = list_lines(
            &heading(None),
            &merged(sample_rows(), false),
            true,
            &clock(),
        );
        assert_eq!(
            texts(&lines),
            [
                "# Workflow runs",
                "demo (production)",
                "ID        WORKFLOW    STATUS   ATTEMPTS  SERVER  WHEN             ERROR",
                "run_live  send-email  running  1/3       nyc     running",
                "run_next  send-email  pending  1/5       la      next in 5m",
                "run_dead  send-email  dead     3/3       la      finished 2h ago  SMTP timeout at send (mail.ts:10)",
            ]
        );
    }

    #[test]
    fn list_without_server_column_and_with_more_runs_hint() {
        let lines = list_lines(
            &heading(None),
            &merged(sample_rows()[2..].to_vec(), true),
            false,
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
        let lines = list_lines(&heading(None), &merged(Vec::new(), false), false, &clock());
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
        let lines = show_lines(&heading(Some("la")), &sample_detail(), &clock());
        assert_eq!(
            texts(&lines),
            [
                "# Workflow run",
                "demo (production) on la",
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
        run.waits = vec![WorkflowRunWait {
            step_name: "approval".into(),
            event_name: "order.approved".into(),
            expires_at_ms: None,
        }];
        let lines = show_lines(&heading(None), &run, &clock());
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
            ]
        );
    }

    fn transition(
        store: WorkflowStoreKind,
        previous: WorkflowRunStatus,
        status: WorkflowRunStatus,
    ) -> WorkflowRunTransition {
        WorkflowRunTransition {
            app: "demo/production".into(),
            store,
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
                &transition(
                    WorkflowStoreKind::Sqlite,
                    WorkflowRunStatus::Dead,
                    WorkflowRunStatus::Pending
                ),
                Some("la")
            ),
            "Retried run_dead on la: dead -> pending"
        );
        assert_eq!(
            transition_line(
                "Cancelled",
                &transition(
                    WorkflowStoreKind::Postgres,
                    WorkflowRunStatus::Running,
                    WorkflowRunStatus::Cancelled
                ),
                None
            ),
            "Cancelled run_dead: running -> cancelled"
        );
    }

    #[test]
    fn json_shapes_are_stable() {
        let mut list = merged(sample_rows()[2..].to_vec(), false);
        list.errors.push(ServerError {
            name: "nyc".into(),
            error: "connection refused".into(),
        });
        let value = list_json("demo", "production", &list);
        assert_eq!(
            value,
            json!({
                "ok": true,
                "command": "workflows",
                "app": "demo",
                "environment": "production",
                "store": "sqlite",
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
                    "server": "la",
                }],
                "has_more": false,
                "errors": [{"name": "nyc", "error": "connection refused"}],
            })
        );

        let found = FoundRun {
            server: "la".into(),
            store: WorkflowStoreKind::Postgres,
            run: detail("run_dead"),
            errors: Vec::new(),
        };
        let value = show_json("demo", "production", &found);
        assert_eq!(value["server"], Value::Null);
        assert_eq!(value["store"], "postgres");
        assert_eq!(value["run"]["payload"]["value"]["to"], "a@example.com");
        assert_eq!(value["run"]["steps_total"], 0);

        let value = transition_json(
            "demo",
            "production",
            &FoundRun {
                store: WorkflowStoreKind::Sqlite,
                ..found
            },
            &transition(
                WorkflowStoreKind::Sqlite,
                WorkflowRunStatus::Pending,
                WorkflowRunStatus::Cancelled,
            ),
        );
        assert_eq!(
            value,
            json!({
                "ok": true,
                "command": "workflows",
                "app": "demo",
                "environment": "production",
                "store": "sqlite",
                "server": "la",
                "id": "run_dead",
                "previous_status": "pending",
                "status": "cancelled",
                "errors": [],
            })
        );
    }
}
