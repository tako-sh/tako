//! Combine per-server workflow replies into one answer.
//!
//! A SQLite store belongs to one server, so its runs are labelled with that
//! server. A Postgres store is shared by every server of the environment, so
//! each server returns the same runs: they are de-duplicated by run ID and
//! carry no server label.

use serde::Serialize;
use tako_core::{
    GetWorkflowRunResponse, ListWorkflowRunsResponse, WorkflowRunDetail, WorkflowRunSummary,
    WorkflowStoreKind,
};

/// A server that could not answer, with its error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ServerError {
    pub name: String,
    pub error: String,
}

/// One run of the merged list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ListedRun {
    #[serde(flatten)]
    pub run: WorkflowRunSummary,
    /// The server whose SQLite store holds the run; `None` for a shared store.
    pub server: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MergedRuns {
    pub store: WorkflowStoreKind,
    pub runs: Vec<ListedRun>,
    pub has_more: bool,
    pub errors: Vec<ServerError>,
}

/// Merge list replies, newest first, keeping at most `limit` runs.
pub(super) fn merge_runs(
    results: Vec<(String, Result<ListWorkflowRunsResponse, String>)>,
    limit: usize,
) -> Result<MergedRuns, String> {
    let mut store = None;
    let mut runs: Vec<ListedRun> = Vec::new();
    let mut has_more = false;
    let mut errors = Vec::new();
    for (server, result) in results {
        match result {
            Ok(response) => {
                store.get_or_insert(response.store);
                has_more |= response.has_more;
                let label = (response.store == WorkflowStoreKind::Sqlite).then_some(server);
                for run in response.runs {
                    let duplicate = label.is_none()
                        && runs
                            .iter()
                            .any(|seen| seen.server.is_none() && seen.run.id == run.id);
                    if !duplicate {
                        runs.push(ListedRun {
                            run,
                            server: label.clone(),
                        });
                    }
                }
            }
            Err(error) => errors.push(ServerError {
                name: server,
                error,
            }),
        }
    }
    let Some(store) = store else {
        return Err(all_failed(&errors));
    };
    runs.sort_by(|a, b| {
        b.run
            .created_at_ms
            .cmp(&a.run.created_at_ms)
            .then_with(|| b.run.id.cmp(&a.run.id))
            .then_with(|| a.server.cmp(&b.server))
    });
    if runs.len() > limit {
        runs.truncate(limit);
        has_more = true;
    }
    Ok(MergedRuns {
        store,
        runs,
        has_more,
        errors,
    })
}

/// The one server answering for a run.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct FoundRun {
    /// The server to send follow-up commands to.
    pub server: String,
    pub store: WorkflowStoreKind,
    pub run: WorkflowRunDetail,
    pub errors: Vec<ServerError>,
}

impl FoundRun {
    /// The per-server label for output; `None` for a shared store.
    pub fn server_label(&self) -> Option<&str> {
        (self.store == WorkflowStoreKind::Sqlite).then_some(self.server.as_str())
    }
}

/// Find the single server that holds run `id`.
///
/// A shared store answers from any server, so the first reply wins. Separate
/// SQLite stores can each hold a run with the same ID; that is refused so a
/// retry or cancel never acts on the wrong one.
pub(super) fn locate_run(
    id: &str,
    results: Vec<(String, Result<GetWorkflowRunResponse, String>)>,
) -> Result<FoundRun, String> {
    let mut hits = Vec::new();
    let mut answered = Vec::new();
    let mut errors = Vec::new();
    for (server, result) in results {
        match result {
            Ok(response) => {
                answered.push(server.clone());
                if let Some(run) = response.run {
                    hits.push((server, response.store, run));
                }
            }
            Err(error) => errors.push(ServerError {
                name: server,
                error,
            }),
        }
    }
    if answered.is_empty() {
        return Err(all_failed(&errors));
    }
    if let Some(index) = hits
        .iter()
        .position(|(_, store, _)| *store == WorkflowStoreKind::Postgres)
    {
        let (server, store, run) = hits.swap_remove(index);
        return Ok(FoundRun {
            server,
            store,
            run,
            errors,
        });
    }
    match hits.len() {
        0 if errors.is_empty() => Err(format!("Workflow run not found: {id}")),
        0 => Err(format!(
            "Workflow run not found: {id}. Could not check {}",
            format_errors(&errors)
        )),
        1 => {
            let (server, store, run) = hits.remove(0);
            Ok(FoundRun {
                server,
                store,
                run,
                errors,
            })
        }
        _ => {
            let servers: Vec<&str> = hits.iter().map(|(name, _, _)| name.as_str()).collect();
            Err(format!(
                "Workflow run {id} exists on several servers: {}. Pass --server <name> to choose one.",
                servers.join(", ")
            ))
        }
    }
}

fn all_failed(errors: &[ServerError]) -> String {
    if errors.is_empty() {
        "No target servers found.".to_string()
    } else {
        format!(
            "Failed to query workflow runs from all target servers: {}",
            format_errors(errors)
        )
    }
}

fn format_errors(errors: &[ServerError]) -> String {
    errors
        .iter()
        .map(|error| format!("{} ({})", error.name, error.error))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use tako_core::{CappedJson, WorkflowRunStatus};

    pub(crate) fn summary(id: &str, created_at_ms: i64) -> WorkflowRunSummary {
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
                value: serde_json::json!({"to": "a@example.com"}),
                truncated: false,
                bytes: 22,
                preview: None,
            },
            result: None,
            steps: Vec::new(),
            steps_total: 0,
            waits: Vec::new(),
        }
    }

    fn list(
        store: WorkflowStoreKind,
        runs: Vec<WorkflowRunSummary>,
        has_more: bool,
    ) -> Result<ListWorkflowRunsResponse, String> {
        Ok(ListWorkflowRunsResponse {
            app: "demo/production".to_string(),
            store,
            runs,
            has_more,
        })
    }

    fn get(
        store: WorkflowStoreKind,
        run: Option<WorkflowRunDetail>,
    ) -> Result<GetWorkflowRunResponse, String> {
        Ok(GetWorkflowRunResponse {
            app: "demo/production".to_string(),
            store,
            run,
        })
    }

    #[test]
    fn sqlite_runs_are_labelled_by_server_and_sorted_newest_first() {
        let merged = merge_runs(
            vec![
                (
                    "la".into(),
                    list(
                        WorkflowStoreKind::Sqlite,
                        vec![summary("a", 30), summary("b", 10)],
                        false,
                    ),
                ),
                (
                    "nyc".into(),
                    list(
                        WorkflowStoreKind::Sqlite,
                        vec![summary("c", 20), summary("a", 5)],
                        false,
                    ),
                ),
            ],
            50,
        )
        .unwrap();
        let order: Vec<(&str, Option<&str>)> = merged
            .runs
            .iter()
            .map(|row| (row.run.id.as_str(), row.server.as_deref()))
            .collect();
        assert_eq!(
            order,
            [
                ("a", Some("la")),
                ("c", Some("nyc")),
                ("b", Some("la")),
                ("a", Some("nyc")),
            ]
        );
        assert_eq!(merged.store, WorkflowStoreKind::Sqlite);
        assert!(!merged.has_more);
    }

    #[test]
    fn shared_postgres_runs_are_deduplicated_without_server_labels() {
        let runs = vec![summary("a", 30), summary("b", 10)];
        let merged = merge_runs(
            vec![
                (
                    "la".into(),
                    list(WorkflowStoreKind::Postgres, runs.clone(), true),
                ),
                ("nyc".into(), list(WorkflowStoreKind::Postgres, runs, true)),
            ],
            50,
        )
        .unwrap();
        assert_eq!(merged.runs.len(), 2);
        assert!(merged.runs.iter().all(|row| row.server.is_none()));
        assert_eq!(merged.store, WorkflowStoreKind::Postgres);
        assert!(merged.has_more);
    }

    #[test]
    fn merged_list_is_capped_at_the_limit_and_reports_more() {
        let merged = merge_runs(
            vec![
                (
                    "la".into(),
                    list(WorkflowStoreKind::Sqlite, vec![summary("a", 3)], false),
                ),
                (
                    "nyc".into(),
                    list(
                        WorkflowStoreKind::Sqlite,
                        vec![summary("b", 2), summary("c", 1)],
                        false,
                    ),
                ),
            ],
            2,
        )
        .unwrap();
        assert_eq!(merged.runs.len(), 2);
        assert!(merged.has_more);
    }

    #[test]
    fn partial_failures_are_kept_and_total_failure_is_an_error() {
        let merged = merge_runs(
            vec![
                ("la".into(), Err("connection refused".into())),
                (
                    "nyc".into(),
                    list(WorkflowStoreKind::Sqlite, vec![summary("a", 1)], false),
                ),
            ],
            50,
        )
        .unwrap();
        assert_eq!(
            merged.errors,
            [ServerError {
                name: "la".into(),
                error: "connection refused".into()
            }]
        );
        let error = merge_runs(vec![("la".into(), Err("connection refused".into()))], 50)
            .expect_err("all servers failed");
        assert_eq!(
            error,
            "Failed to query workflow runs from all target servers: la (connection refused)"
        );
    }

    #[test]
    fn locate_finds_the_only_sqlite_server_with_the_run() {
        let found = locate_run(
            "a",
            vec![
                ("la".into(), get(WorkflowStoreKind::Sqlite, None)),
                (
                    "nyc".into(),
                    get(WorkflowStoreKind::Sqlite, Some(detail("a"))),
                ),
            ],
        )
        .unwrap();
        assert_eq!(found.server, "nyc");
        assert_eq!(found.server_label(), Some("nyc"));
    }

    #[test]
    fn locate_uses_one_server_for_a_shared_store() {
        let found = locate_run(
            "a",
            vec![
                (
                    "la".into(),
                    get(WorkflowStoreKind::Postgres, Some(detail("a"))),
                ),
                (
                    "nyc".into(),
                    get(WorkflowStoreKind::Postgres, Some(detail("a"))),
                ),
            ],
        )
        .unwrap();
        assert_eq!(found.server, "la");
        assert_eq!(found.server_label(), None);
    }

    #[test]
    fn locate_refuses_a_run_id_held_by_two_sqlite_servers() {
        let error = locate_run(
            "a",
            vec![
                (
                    "la".into(),
                    get(WorkflowStoreKind::Sqlite, Some(detail("a"))),
                ),
                (
                    "nyc".into(),
                    get(WorkflowStoreKind::Sqlite, Some(detail("a"))),
                ),
            ],
        )
        .expect_err("ambiguous");
        assert_eq!(
            error,
            "Workflow run a exists on several servers: la, nyc. Pass --server <name> to choose one."
        );
    }

    #[test]
    fn locate_reports_missing_runs_and_unreachable_servers() {
        assert_eq!(
            locate_run(
                "a",
                vec![("la".into(), get(WorkflowStoreKind::Sqlite, None))]
            )
            .expect_err("missing"),
            "Workflow run not found: a"
        );
        assert_eq!(
            locate_run(
                "a",
                vec![
                    ("la".into(), get(WorkflowStoreKind::Sqlite, None)),
                    ("nyc".into(), Err("timed out".into())),
                ]
            )
            .expect_err("missing"),
            "Workflow run not found: a. Could not check nyc (timed out)"
        );
        assert_eq!(
            locate_run("a", vec![("nyc".into(), Err("timed out".into()))]).expect_err("failed"),
            "Failed to query workflow runs from all target servers: nyc (timed out)"
        );
    }
}
