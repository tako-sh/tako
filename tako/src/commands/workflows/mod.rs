//! `tako workflows`: inspect and repair an environment's workflow runs.
//!
//! Every mapped server is asked. Per-server SQLite stores answer for their own
//! runs, which are labelled with the server; a shared Postgres store answers
//! the same from every server, so its runs are de-duplicated. Retry and cancel
//! first locate the run, then act on exactly one server.

use std::path::Path;

use clap::{Subcommand, ValueEnum};
use tako_core::{
    Command, GetWorkflowRunResponse, ListWorkflowRunsResponse, WORKFLOW_RUNS_DEFAULT_LIMIT,
    WORKFLOW_RUNS_MAX_LIMIT, WorkflowRunStatus, WorkflowRunTransition,
};

use crate::app::require_app_name_from_config_path;
use crate::commands::backups::client::{send_typed_to_server, send_typed_to_servers};
use crate::commands::backups::target::{BackupTarget as Target, resolve_backup_target};
use crate::commands::project_context;
use crate::config::{ServersToml, TakoToml};
use crate::output;

mod merge;
mod render;

use merge::{FoundRun, locate_run, merge_runs};
use render::{Clock, Heading, emit};

#[derive(Subcommand)]
pub enum WorkflowCommands {
    /// List workflow runs, newest first
    #[command(visible_alias = "ls")]
    List {
        /// Environment to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Specific server to query
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
        /// Run id from `tako workflows list`
        run_id: String,
        /// Environment to query (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Specific server to query
        #[arg(long)]
        server: Option<String>,
    },

    /// Return a dead or cancelled run to pending with a fresh retry budget
    Retry {
        /// Run id from `tako workflows list`
        run_id: String,
        /// Environment of the run (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Specific server holding the run
        #[arg(long)]
        server: Option<String>,
    },

    /// Cancel a pending or running run
    Cancel {
        /// Run id from `tako workflows list`
        run_id: String,
        /// Environment of the run (defaults to production)
        #[arg(long)]
        env: Option<String>,
        /// Specific server holding the run
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
    let servers = super::helpers::load_management_servers(
        Some(&tako_config),
        Some(env.unwrap_or("production")),
    )
    .await?;
    let target = resolve_backup_target(&app_name, env, server, &tako_config, &servers)?;

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

/// Show which server a result came from only when the environment has several.
fn shown_server<'a>(target: &Target, server: Option<&'a str>) -> Option<&'a str> {
    server.filter(|_| target.server_names.len() > 1)
}

fn warn_unreachable(errors: &[merge::ServerError]) {
    for error in errors {
        output::warning(&format!(
            "{}: failed to load workflow runs ({})",
            error.name, error.error
        ));
    }
}

async fn list_runs(
    target: &Target,
    servers: &ServersToml,
    status: Option<WorkflowRunStatus>,
    name: Option<String>,
    limit: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let limit = limit.unwrap_or(WORKFLOW_RUNS_DEFAULT_LIMIT);
    let results = {
        let _t = output::timed("Load workflow runs");
        send_typed_to_servers::<ListWorkflowRunsResponse>(
            &target.server_names,
            servers,
            Command::ListWorkflowRuns {
                app: target.remote_app_name.clone(),
                status,
                name,
                limit: Some(limit),
            },
            "list_workflow_runs",
            &format!(
                "Loading workflow runs from {} server(s)",
                target.server_names.len()
            ),
        )
        .await?
    };
    let merged = merge_runs(results, limit as usize)?;

    if output::is_json() {
        return output::json_result(render::list_json(&target.app_name, &target.env, &merged));
    }

    warn_unreachable(&merged.errors);
    let show_server =
        target.server_names.len() > 1 && merged.runs.iter().any(|row| row.server.is_some());
    emit(&render::list_lines(
        &Heading {
            app: &target.app_name,
            env: &target.env,
            server: None,
        },
        &merged,
        show_server,
        &Clock::local(),
    ));
    Ok(())
}

/// Ask every target server for the run and pick the one that holds it.
async fn find_run(
    target: &Target,
    servers: &ServersToml,
    run_id: &str,
) -> Result<FoundRun, Box<dyn std::error::Error>> {
    let _t = output::timed(&format!("Find workflow run {run_id}"));
    let results = send_typed_to_servers::<GetWorkflowRunResponse>(
        &target.server_names,
        servers,
        Command::GetWorkflowRun {
            app: target.remote_app_name.clone(),
            id: run_id.to_string(),
        },
        "get_workflow_run",
        &format!(
            "Loading workflow run from {} server(s)",
            target.server_names.len()
        ),
    )
    .await?;
    Ok(locate_run(run_id, results)?)
}

async fn show_run(
    target: &Target,
    servers: &ServersToml,
    run_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let found = find_run(target, servers, run_id).await?;
    if output::is_json() {
        return output::json_result(render::show_json(&target.app_name, &target.env, &found));
    }
    warn_unreachable(&found.errors);
    emit(&render::show_lines(
        &Heading {
            app: &target.app_name,
            env: &target.env,
            server: shown_server(target, found.server_label()),
        },
        &found.run,
        &Clock::local(),
    ));
    Ok(())
}

async fn retry_run(
    target: &Target,
    servers: &ServersToml,
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
    servers: &ServersToml,
    run_id: &str,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let found = find_run(target, servers, run_id).await?;
    if should_confirm_cancel(found.run.status, assume_yes, output::is_interactive()) {
        let confirmed = output::confirm_with_description(
            &format!(
                "Cancel workflow run {} ({}, {}) in {}?",
                output::strong(run_id),
                found.run.name,
                found.run.status,
                output::strong(&target.env)
            ),
            (found.run.status == WorkflowRunStatus::Running)
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
/// `backups restore`. A run that cannot be cancelled goes straight to the
/// server, which explains why.
fn should_confirm_cancel(status: WorkflowRunStatus, assume_yes: bool, interactive: bool) -> bool {
    !assume_yes
        && interactive
        && matches!(
            status,
            WorkflowRunStatus::Pending | WorkflowRunStatus::Running
        )
}

async fn transition(
    target: &Target,
    servers: &ServersToml,
    found: &FoundRun,
    command: Command,
    action: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = servers
        .get(&found.server)
        .ok_or_else(|| format!("Server '{}' not found in config.toml", found.server))?;
    if !output::is_json() {
        warn_unreachable(&found.errors);
    }
    if output::is_dry_run() {
        output::dry_run_skip(&format!("{action} workflow run {}", found.run.id));
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
            found,
            &transition,
        ));
    }
    output::success(&render::transition_line(
        action,
        &transition,
        shown_server(target, found.server_label()),
    ));
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
