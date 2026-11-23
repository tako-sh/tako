//! `tako workflow`: inspect and repair an environment's workflow runs.
//!
//! Every server of an environment reads the same hosted workflow store, so
//! one answer is enough: servers are asked in order and the first that
//! answers is used. Retry and cancel first read the run, then act on the
//! server that answered.

use std::path::Path;

use clap::{Subcommand, ValueEnum};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tako_core::{
    Command, GetWorkflowRunResponse, ListWorkflowRunsResponse, WORKFLOW_RUNS_MAX_LIMIT,
    WorkflowRunDetail, WorkflowRunStatus, WorkflowRunTransition,
};
use tracing::Instrument;

use crate::app::require_app_name_from_config_path;
use crate::commands::project_context;
use crate::commands::server_target::{
    EnvTarget as Target, resolve_env_target, send_typed_to_server,
};
use crate::config::{ServerInventory, TakoToml};
use crate::output;

mod render;

use render::{Clock, Heading, emit};

#[derive(Subcommand)]
pub enum WorkflowCommands {
    /// List workflow runs, newest first
    #[command(visible_alias = "ls")]
    List {
        /// Environment to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Server to ask (defaults to the first reachable one)
        #[arg(long)]
        server: Option<String>,
        /// Only runs with this status
        #[arg(long, value_enum)]
        status: Option<RunStatusArg>,
        /// Only runs of this workflow
        #[arg(long)]
        name: Option<String>,
        /// Maximum number of runs to show, 1-200 (default 50)
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=WORKFLOW_RUNS_MAX_LIMIT as i64))]
        limit: Option<u32>,
    },

    /// Show a run with its saved steps, waits, and error
    Show {
        /// Run id from `tako workflow list`
        run_id: String,
        /// Environment to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Server to ask (defaults to the first reachable one)
        #[arg(long)]
        server: Option<String>,
    },

    /// Return a dead or cancelled run to pending with a fresh retry budget
    Retry {
        /// Run id from `tako workflow list`
        run_id: String,
        /// Environment of the run (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Server to ask (defaults to the first reachable one)
        #[arg(long)]
        server: Option<String>,
    },

    /// Cancel a pending or running run
    Cancel {
        /// Run id from `tako workflow list`
        run_id: String,
        /// Environment of the run (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Server to ask (defaults to the first reachable one)
        #[arg(long)]
        server: Option<String>,
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

/// A workflow run status accepted by `--status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RunStatusArg {
    Pending,
    Running,
    Succeeded,
    Cancelled,
    Dead,
}

impl From<RunStatusArg> for WorkflowRunStatus {
    fn from(value: RunStatusArg) -> Self {
        match value {
            RunStatusArg::Pending => Self::Pending,
            RunStatusArg::Running => Self::Running,
            RunStatusArg::Succeeded => Self::Succeeded,
            RunStatusArg::Cancelled => Self::Cancelled,
            RunStatusArg::Dead => Self::Dead,
        }
    }
}

impl WorkflowCommands {
    fn target_args(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Self::List { env, server, .. }
            | Self::Show { env, server, .. }
            | Self::Retry { env, server, .. }
            | Self::Cancel { env, server, .. } => (env.as_deref(), server.as_deref()),
        }
    }
}

pub fn run(
    cmd: WorkflowCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd, config_path))
}

async fn run_async(
    cmd: WorkflowCommands,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (env, server) = cmd.target_args();
    reject_development(env)?;
    let context = project_context::resolve_existing(config_path)?;
    let app_name = require_app_name_from_config_path(&context.config_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let tako_config = TakoToml::load_from_file(&context.config_path)?;
    let servers = super::env_network::servers_for_target(
        Some(&context.config_path),
        Some(&tako_config),
        Some(env.unwrap_or("production")),
    )
    .await?;
    let target = resolve_env_target(&app_name, env, server, &tako_config, &servers)?;

    match cmd {
        WorkflowCommands::List {
            status,
            name,
            limit,
            ..
        } => list_runs(&target, &servers, status.map(Into::into), name, limit).await,
        WorkflowCommands::Show { run_id, .. } => show_run(&target, &servers, &run_id).await,
        WorkflowCommands::Retry { run_id, .. } => retry_run(&target, &servers, &run_id).await,
        WorkflowCommands::Cancel { run_id, yes, .. } => {
            cancel_run(&target, &servers, &run_id, yes).await
        }
    }
}

fn reject_development(env: Option<&str>) -> Result<(), String> {
    if env == Some("development") {
        return Err(
            "Environment 'development' is reserved for local development. Workflow run commands only work against deployed environments."
                .to_string(),
        );
    }
    Ok(())
}

/// A server that did not answer, with its error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ServerError {
    name: String,
    error: String,
}

/// The reply of the first server that answered, with the servers that did
/// not answer before it.
#[derive(Debug, PartialEq)]
struct Answer<T> {
    server: String,
    reply: T,
    errors: Vec<ServerError>,
}

/// Ask `server_names` in order and return the first answer.
async fn first_answer<T>(
    server_names: &[String],
    mut ask: impl AsyncFnMut(&str) -> Result<T, String>,
) -> Result<Answer<T>, String> {
    let mut errors = Vec::new();
    for name in server_names {
        match ask(name).await {
            Ok(reply) => {
                return Ok(Answer {
                    server: name.clone(),
                    reply,
                    errors,
                });
            }
            Err(error) => errors.push(ServerError {
                name: name.clone(),
                error,
            }),
        }
    }
    Err(if errors.is_empty() {
        "No target servers found.".to_string()
    } else {
        format!(
            "Failed to query workflow runs from all target servers: {}",
            errors
                .iter()
                .map(|error| format!("{} ({})", error.name, error.error))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Send `command` to the target's servers in order until one answers. Servers
/// skipped on the way are reported as warnings (in JSON, as `errors`).
async fn ask<T: DeserializeOwned>(
    target: &Target,
    servers: &ServerInventory,
    command: Command,
    response_name: &'static str,
    loading: &str,
) -> Result<Answer<T>, String> {
    let answer = output::with_spinner_async_simple(
        loading,
        first_answer(&target.server_names, async |name: &str| {
            let server = servers.get(name).ok_or_else(|| {
                format!("Server '{name}' not found in enrolled network membership")
            })?;
            send_typed_to_server::<T>(server, command.clone(), response_name)
                .instrument(output::scope(name))
                .await
        }),
    )
    .await?;
    if !output::is_json() {
        for error in &answer.errors {
            output::warning(&format!(
                "{}: failed to load workflow runs ({})",
                error.name, error.error
            ));
        }
    }
    Ok(answer)
}

async fn list_runs(
    target: &Target,
    servers: &ServerInventory,
    status: Option<WorkflowRunStatus>,
    name: Option<String>,
    limit: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let answer = {
        let _t = output::timed("Load workflow runs");
        ask::<ListWorkflowRunsResponse>(
            target,
            servers,
            Command::ListWorkflowRuns {
                app: target.remote_app_name.clone(),
                status,
                name,
                limit,
            },
            "list_workflow_runs",
            "Loading workflow runs",
        )
        .await?
    };

    if output::is_json() {
        return output::json_result(render::list_json(
            &target.app_name,
            &target.env,
            &answer.reply,
            &answer.errors,
        ));
    }
    emit(&render::list_lines(
        &Heading {
            app: &target.app_name,
            env: &target.env,
        },
        &answer.reply,
        &Clock::local(),
    ));
    Ok(())
}

/// Read run `id` from the first server that answers.
async fn find_run(
    target: &Target,
    servers: &ServerInventory,
    run_id: &str,
) -> Result<Answer<WorkflowRunDetail>, Box<dyn std::error::Error>> {
    let _t = output::timed(&format!("Find workflow run {run_id}"));
    let answer = ask::<GetWorkflowRunResponse>(
        target,
        servers,
        Command::GetWorkflowRun {
            app: target.remote_app_name.clone(),
            id: run_id.to_string(),
        },
        "get_workflow_run",
        "Loading workflow run",
    )
    .await?;
    Ok(found_run(run_id, answer)?)
}

fn found_run(
    run_id: &str,
    answer: Answer<GetWorkflowRunResponse>,
) -> Result<Answer<WorkflowRunDetail>, String> {
    let Some(run) = answer.reply.run else {
        return Err(format!("Workflow run not found: {run_id}"));
    };
    Ok(Answer {
        server: answer.server,
        reply: run,
        errors: answer.errors,
    })
}

async fn show_run(
    target: &Target,
    servers: &ServerInventory,
    run_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let found = find_run(target, servers, run_id).await?;
    if output::is_json() {
        return output::json_result(render::show_json(&target.app_name, &target.env, &found));
    }
    emit(&render::show_lines(
        &Heading {
            app: &target.app_name,
            env: &target.env,
        },
        &found.reply,
        &Clock::local(),
    ));
    Ok(())
}

async fn retry_run(
    target: &Target,
    servers: &ServerInventory,
    run_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let found = find_run(target, servers, run_id).await?;
    let command = Command::RetryWorkflowRun {
        app: target.remote_app_name.clone(),
        id: run_id.to_string(),
    };
    transition(target, servers, &found, command, "Retried").await
}

async fn cancel_run(
    target: &Target,
    servers: &ServerInventory,
    run_id: &str,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let found = find_run(target, servers, run_id).await?;
    let run = &found.reply;
    if should_confirm_cancel(run.status, assume_yes, output::is_interactive()) {
        let confirmed = output::confirm_with_description(
            &format!(
                "Cancel workflow run {} ({}, {}) in {}?",
                output::strong(run_id),
                run.name,
                run.status,
                output::strong(&target.env)
            ),
            (run.status == WorkflowRunStatus::Running)
                .then_some("The handler stops at its next checkpoint."),
            false,
        )?;
        if !confirmed {
            return Err(output::operation_cancelled_error().into());
        }
    }
    let command = Command::CancelWorkflowRun {
        app: target.remote_app_name.clone(),
        id: run_id.to_string(),
    };
    transition(target, servers, &found, command, "Cancelled").await
}

/// Cancel stops work, so it asks first in an interactive terminal, like
/// `db restore`. A run that cannot be cancelled goes straight to the
/// server, which explains why.
fn should_confirm_cancel(status: WorkflowRunStatus, assume_yes: bool, interactive: bool) -> bool {
    !assume_yes
        && interactive
        && matches!(
            status,
            WorkflowRunStatus::Pending | WorkflowRunStatus::Running
        )
}

/// Send a retry or cancel to the server that answered for the run.
async fn transition(
    target: &Target,
    servers: &ServerInventory,
    found: &Answer<WorkflowRunDetail>,
    command: Command,
    action: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = servers.get(&found.server).ok_or_else(|| {
        format!(
            "Server '{}' not found in enrolled network membership",
            found.server
        )
    })?;
    if output::is_dry_run() {
        output::dry_run_skip(&format!("{action} workflow run {}", found.reply.id));
        return Ok(());
    }
    let response_name = match command {
        Command::RetryWorkflowRun { .. } => "retry_workflow_run",
        _ => "cancel_workflow_run",
    };
    let transition = {
        let _t = output::timed(&format!("[{}] {action} workflow run", found.server));
        send_typed_to_server::<WorkflowRunTransition>(server, command, response_name).await?
    };

    if output::is_json() {
        return output::json_result(render::transition_json(
            &target.app_name,
            &target.env,
            &transition,
            &found.errors,
        ));
    }
    output::success(&render::transition_line(action, &transition));
    if transition.previous_status == WorkflowRunStatus::Running
        && transition.status == WorkflowRunStatus::Cancelled
    {
        output::hint("The handler stops at its next checkpoint.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_is_refused() {
        assert_eq!(
            reject_development(Some("development")).unwrap_err(),
            "Environment 'development' is reserved for local development. Workflow run commands only work against deployed environments."
        );
        assert!(reject_development(Some("staging")).is_ok());
        assert!(reject_development(None).is_ok());
    }

    #[test]
    fn cancel_confirms_only_interactively_for_cancellable_runs() {
        use WorkflowRunStatus::*;
        assert!(should_confirm_cancel(Running, false, true));
        assert!(should_confirm_cancel(Pending, false, true));
        assert!(!should_confirm_cancel(Running, true, true));
        assert!(!should_confirm_cancel(Running, false, false));
        assert!(!should_confirm_cancel(Dead, false, true));
    }

    /// Answers from a scripted list of replies, recording who was asked.
    async fn answer_from(
        replies: &[(&str, Result<u32, &str>)],
    ) -> (Result<Answer<u32>, String>, Vec<String>) {
        let names: Vec<String> = replies.iter().map(|(name, _)| name.to_string()).collect();
        let mut asked = Vec::new();
        let answer = first_answer(&names, async |name: &str| {
            asked.push(name.to_string());
            let (_, reply) = replies.iter().find(|(server, _)| *server == name).unwrap();
            reply.map_err(str::to_string)
        })
        .await;
        (answer, asked)
    }

    #[tokio::test]
    async fn the_first_server_that_answers_is_used_and_later_ones_are_not_asked() {
        let (answer, asked) = answer_from(&[
            ("la", Err("connection refused")),
            ("nyc", Ok(7)),
            ("sfo", Ok(8)),
        ])
        .await;
        assert_eq!(
            answer.unwrap(),
            Answer {
                server: "nyc".into(),
                reply: 7,
                errors: vec![ServerError {
                    name: "la".into(),
                    error: "connection refused".into(),
                }],
            }
        );
        assert_eq!(asked, ["la", "nyc"]);
    }

    #[tokio::test]
    async fn no_answer_reports_every_server() {
        let (answer, asked) =
            answer_from(&[("la", Err("connection refused")), ("nyc", Err("timed out"))]).await;
        assert_eq!(
            answer.unwrap_err(),
            "Failed to query workflow runs from all target servers: la (connection refused), nyc (timed out)"
        );
        assert_eq!(asked, ["la", "nyc"]);
        assert_eq!(
            answer_from(&[]).await.0.unwrap_err(),
            "No target servers found."
        );
    }

    #[test]
    fn found_run_requires_the_run() {
        let answer = |run| Answer {
            server: "la".to_string(),
            reply: GetWorkflowRunResponse {
                app: "demo/production".to_string(),
                run,
            },
            errors: Vec::new(),
        };
        assert_eq!(
            found_run("a", answer(None)).unwrap_err(),
            "Workflow run not found: a"
        );
        let found = found_run("a", answer(Some(render::tests::detail("a")))).unwrap();
        assert_eq!(found.server, "la");
        assert_eq!(found.reply.id, "a");
    }

    #[test]
    fn status_arguments_map_to_every_run_status() {
        for (arg, status) in [
            (RunStatusArg::Pending, "pending"),
            (RunStatusArg::Running, "running"),
            (RunStatusArg::Succeeded, "succeeded"),
            (RunStatusArg::Cancelled, "cancelled"),
            (RunStatusArg::Dead, "dead"),
        ] {
            assert_eq!(WorkflowRunStatus::from(arg).as_str(), status);
        }
    }
}
