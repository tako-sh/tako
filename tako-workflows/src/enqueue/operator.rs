//! Operator views of runs: rows read by the store backends, capped into the
//! management reply types.

use tako_core::{
    CappedJson, WORKFLOW_LIST_ERROR_MAX_BYTES, WORKFLOW_RUN_ERROR_MAX_BYTES,
    WORKFLOW_RUN_VALUE_MAX_BYTES, WORKFLOW_STEP_VALUE_MAX_BYTES, WorkflowRunDetail,
    WorkflowRunStatus, WorkflowRunStep, WorkflowRunSummary, WorkflowRunWait,
};

use super::RunsDbError;

/// Stored as the error of a run an operator cancels.
pub const OPERATOR_CANCEL_REASON: &str = "cancelled by operator";

/// Serialized `GetWorkflowRun` replies stay below this, well inside the 1 MiB
/// JSONL frame, after the per-field caps. Older steps are dropped to fit.
const RUN_DETAIL_BUDGET_BYTES: usize = 768 * 1024;

/// One `runs` row without its payload or result.
pub(crate) struct SummaryRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) attempts: i64,
    pub(crate) max_attempts: i64,
    pub(crate) run_at: i64,
    pub(crate) created_at: i64,
    pub(crate) finished_at: Option<i64>,
    pub(crate) last_error: Option<String>,
}

pub(crate) struct RunRow {
    pub(crate) summary: SummaryRow,
    pub(crate) payload: String,
    pub(crate) result: Option<String>,
    pub(crate) lease_until: Option<i64>,
    pub(crate) worker_id: Option<String>,
}

pub(crate) struct StepRow {
    pub(crate) name: String,
    pub(crate) result: String,
    pub(crate) completed_at: i64,
}

pub(crate) struct WaitRow {
    pub(crate) step_name: String,
    pub(crate) event_name: String,
    pub(crate) expires_at: Option<i64>,
}

/// A run with its most recent steps (newest first, as the backends read
/// them), the number of steps stored, and its event waits.
pub(crate) struct RunRecord {
    pub(crate) run: RunRow,
    pub(crate) steps: Vec<StepRow>,
    pub(crate) steps_total: u64,
    pub(crate) waits: Vec<WaitRow>,
}

/// Outcome of an operator cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelledRun {
    pub previous_status: WorkflowRunStatus,
    /// Claim owner of the attempt that was fenced, when the run was running.
    pub worker_id: Option<String>,
}

pub(crate) fn parse_status(status: &str) -> Result<WorkflowRunStatus, RunsDbError> {
    status
        .parse()
        .map_err(|error: String| RunsDbError::Storage(error))
}

/// Text cut to at most `max` bytes at a UTF-8 boundary, and whether it was cut.
pub(crate) fn cap_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

pub(crate) fn cap_json(text: &str, max: usize) -> CappedJson {
    let bytes = text.len() as u64;
    if text.len() <= max {
        return CappedJson {
            value: serde_json::from_str(text).unwrap_or(serde_json::Value::Null),
            truncated: false,
            bytes,
            preview: None,
        };
    }
    CappedJson {
        value: serde_json::Value::Null,
        truncated: true,
        bytes,
        preview: Some(cap_text(text, max).0),
    }
}

/// Parked-until-signal runs store `i64::MAX`; they have no due time.
fn due_time(run_at: i64) -> Option<i64> {
    (run_at != i64::MAX).then_some(run_at)
}

fn capped_error(error: Option<&str>, max: usize) -> (Option<String>, bool) {
    match error {
        Some(error) => {
            let (error, truncated) = cap_text(error, max);
            (Some(error), truncated)
        }
        None => (None, false),
    }
}

impl SummaryRow {
    pub(crate) fn into_summary(self) -> Result<WorkflowRunSummary, RunsDbError> {
        let (last_error, last_error_truncated) =
            capped_error(self.last_error.as_deref(), WORKFLOW_LIST_ERROR_MAX_BYTES);
        Ok(WorkflowRunSummary {
            status: parse_status(&self.status)?,
            id: self.id,
            name: self.name,
            attempts: self.attempts.max(0) as u32,
            max_attempts: self.max_attempts.max(0) as u32,
            created_at_ms: self.created_at,
            run_at_ms: due_time(self.run_at),
            finished_at_ms: self.finished_at,
            last_error,
            last_error_truncated,
        })
    }
}

impl RunRecord {
    pub(crate) fn into_detail(self) -> Result<WorkflowRunDetail, RunsDbError> {
        let RunRecord {
            run,
            steps,
            steps_total,
            waits,
        } = self;
        let summary = run.summary;
        let (last_error, last_error_truncated) =
            capped_error(summary.last_error.as_deref(), WORKFLOW_RUN_ERROR_MAX_BYTES);
        let mut steps: Vec<WorkflowRunStep> = steps
            .into_iter()
            .rev()
            .map(|step| WorkflowRunStep {
                result: cap_json(&step.result, WORKFLOW_STEP_VALUE_MAX_BYTES),
                name: step.name,
                completed_at_ms: step.completed_at,
            })
            .collect();
        let mut detail = WorkflowRunDetail {
            status: parse_status(&summary.status)?,
            id: summary.id,
            name: summary.name,
            attempts: summary.attempts.max(0) as u32,
            max_attempts: summary.max_attempts.max(0) as u32,
            created_at_ms: summary.created_at,
            run_at_ms: due_time(summary.run_at),
            finished_at_ms: summary.finished_at,
            lease_until_ms: run.lease_until,
            worker_id: run.worker_id,
            last_error,
            last_error_truncated,
            payload: cap_json(&run.payload, WORKFLOW_RUN_VALUE_MAX_BYTES),
            result: run
                .result
                .as_deref()
                .map(|result| cap_json(result, WORKFLOW_RUN_VALUE_MAX_BYTES)),
            steps: Vec::new(),
            steps_total,
            waits: waits
                .into_iter()
                .map(|wait| WorkflowRunWait {
                    step_name: wait.step_name,
                    event_name: wait.event_name,
                    expires_at_ms: wait.expires_at,
                })
                .collect(),
        };
        // Long step or event names are not capped individually. Keep the
        // newest steps that fit the reply budget.
        let mut size = serde_json::to_vec(&detail)?.len();
        let mut keep = 0;
        for step in steps.iter().rev() {
            let step_size = serde_json::to_vec(step)?.len() + 1;
            if size + step_size > RUN_DETAIL_BUDGET_BYTES {
                break;
            }
            size += step_size;
            keep += 1;
        }
        detail.steps = steps.split_off(steps.len() - keep);
        Ok(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_cut_at_utf8_boundaries_and_report_stored_size() {
        assert_eq!(cap_text("héllo", 2), ("h".to_string(), true));
        assert_eq!(cap_text("héllo", 3), ("hé".to_string(), true));
        assert_eq!(cap_text("héllo", 6), ("héllo".to_string(), false));

        let small = cap_json(r#"{"a":1}"#, 7);
        assert_eq!(small.value, serde_json::json!({"a": 1}));
        assert!(!small.truncated);
        assert_eq!((small.bytes, small.preview), (7, None));

        let large = cap_json(r#"{"a":"ééé"}"#, 9);
        assert!(large.truncated);
        assert_eq!(large.value, serde_json::Value::Null);
        assert_eq!(large.bytes, 14);
        assert_eq!(large.preview.as_deref(), Some(r#"{"a":"é"#));
    }

    fn row(id: &str) -> SummaryRow {
        SummaryRow {
            id: id.into(),
            name: "work".into(),
            status: "dead".into(),
            attempts: 3,
            max_attempts: 3,
            run_at: i64::MAX,
            created_at: 1,
            finished_at: Some(2),
            last_error: Some("x".repeat(WORKFLOW_RUN_ERROR_MAX_BYTES + 10)),
        }
    }

    #[test]
    fn huge_values_cannot_push_a_run_reply_past_the_frame_limit() {
        let huge = format!("\"{}\"", "\"".repeat(1024 * 1024));
        let step = |index: usize| StepRow {
            // Step names are not capped; the reply budget still holds.
            name: format!("{index}:{}", "n".repeat(16 * 1024)),
            result: huge.clone(),
            completed_at: index as i64,
        };
        let record = RunRecord {
            run: RunRow {
                summary: row("run"),
                payload: huge.clone(),
                result: Some(huge.clone()),
                lease_until: None,
                worker_id: None,
            },
            // Newest first, as the backends return them.
            steps: (0..100).rev().map(step).collect(),
            steps_total: 250,
            waits: Vec::new(),
        };
        let detail = record.into_detail().unwrap();
        let size = serde_json::to_vec(&detail).unwrap().len();
        assert!(size <= RUN_DETAIL_BUDGET_BYTES, "{size}");
        assert!(detail.payload.truncated && detail.result.as_ref().unwrap().truncated);
        assert_eq!(detail.payload.bytes, huge.len() as u64);
        assert!(detail.last_error_truncated);
        assert_eq!(
            detail.last_error.as_ref().unwrap().len(),
            WORKFLOW_RUN_ERROR_MAX_BYTES
        );
        assert_eq!(detail.run_at_ms, None);
        assert_eq!(detail.steps_total, 250);
        // Older steps are dropped first; the rest stay oldest first.
        assert!(!detail.steps.is_empty() && detail.steps.len() < 100);
        assert_eq!(detail.steps.last().unwrap().completed_at_ms, 99);
        assert!(
            detail
                .steps
                .windows(2)
                .all(|pair| pair[0].completed_at_ms < pair[1].completed_at_ms)
        );
        assert!(detail.steps.iter().all(|step| step.result.truncated));

        let summary = row("run").into_summary().unwrap();
        assert!(summary.last_error_truncated);
        assert_eq!(
            summary.last_error.unwrap().len(),
            WORKFLOW_LIST_ERROR_MAX_BYTES
        );
    }
}
