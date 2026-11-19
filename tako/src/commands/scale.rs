use std::path::Path;

use crate::commands::project_context;
use crate::config::{ServerEntry, TakoToml};
use crate::management_http::ManagementClient;
use crate::output;
use tako_core::{Command, Response};
use tracing::Instrument;

pub fn run(
    instances: Option<u8>,
    service: Option<&str>,
    reset: bool,
    env: Option<&str>,
    server: Option<&str>,
    app: Option<&str>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(
        instances,
        service,
        reset,
        env,
        server,
        app,
        config_path,
    ))
}

async fn run_async(
    instances: Option<u8>,
    service: Option<&str>,
    reset: bool,
    env: Option<&str>,
    server: Option<&str>,
    app: Option<&str>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let project_context = project_context::resolve_optional(config_path)?;
    let project_tako = if let Some(context) = project_context.as_ref() {
        Some(TakoToml::load_from_file(&context.config_path)?)
    } else {
        None
    };
    let config_path = project_context
        .as_ref()
        .map(|context| context.config_path.as_path());

    let target = super::app_target::resolve(project_tako.as_ref(), config_path, app, env, server)?;
    let servers = super::env_network::servers_for_target(
        project_context
            .as_ref()
            .map(|context| context.project_dir.as_path()),
        project_tako.as_ref(),
        target.env_name.as_deref(),
    )
    .await?;
    let server_names = super::app_target::server_names(
        project_tako.as_ref(),
        &servers,
        target.env_name.as_deref(),
        server,
    )?;

    let desired = instances
        .map(|n| format!("{n} instance(s)"))
        .unwrap_or_else(|| "Compose default".into());
    let service_label = service.map(|s| format!(" / {s}")).unwrap_or_default();
    output::section("Scale");
    if let Some(env_name) = target.env_name.as_deref() {
        output::info(&format!(
            "{} ({}){service_label} -> {desired}",
            target.display_app_name, env_name
        ));
    } else {
        output::info(&format!(
            "{}{service_label} -> {desired}",
            target.display_app_name
        ));
    }

    if output::is_dry_run() {
        for name in &server_names {
            output::dry_run_skip(&format!(
                "Scale {}{service_label} to {desired} on {name}",
                target.remote_app_name
            ));
        }
        return Ok(());
    }

    let mut tasks = Vec::new();
    for server_name in &server_names {
        let Some(entry) = servers.get(server_name) else {
            return Err(format!(
                "Server '{}' not found in enrolled network membership",
                server_name
            )
            .into());
        };
        let server_name = server_name.clone();
        let entry = entry.clone();
        let app_name = target.remote_app_name.clone();
        let service = service.map(str::to_owned);
        let span = output::scope(&server_name);
        tasks.push(tokio::spawn(
            async move {
                let result = scale_server(&app_name, &entry, instances, service, reset).await;
                (server_name, result)
            }
            .instrument(span),
        ));
    }

    let results = if output::is_interactive() && tasks.len() > 1 {
        output::with_spinner_async_simple(&format!("Scaling {} server(s)", tasks.len()), async {
            let mut results = Vec::new();
            for task in tasks {
                results.push(task.await);
            }
            results
        })
        .await
    } else {
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await);
        }
        results
    };

    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok((server_name, Ok(scale_result))) => {
                let service_label = scale_result
                    .service
                    .as_ref()
                    .map(|s| format!(" / {s}"))
                    .unwrap_or_default();
                output::bullet(&format!(
                    "{server_name}{service_label}: {} instance(s){}",
                    scale_result.instances,
                    if scale_result.stopped {
                        " (stopped; applies on start)"
                    } else {
                        ""
                    }
                ));
                if scale_result.standby_limited {
                    output::warning(&format!(
                        "{server_name}: standby mode limited scale to {} instance(s)",
                        scale_result.instances
                    ));
                }
            }
            Ok((server_name, Err(error))) => {
                output::error(&format!("{server_name}: {error}"));
                failures.push(server_name);
            }
            Err(error) => {
                output::error(&format!("Scale task failed: {}", error));
                failures.push("<task>".to_string());
            }
        }
    }

    if failures.is_empty() {
        output::success("Scale");
        Ok(())
    } else {
        Err(format!("Failed to scale {} server(s)", failures.len()).into())
    }
}

#[derive(Debug)]
struct ScaleResult {
    instances: u8,
    service: Option<String>,
    standby_limited: bool,
    stopped: bool,
}

async fn scale_server(
    app_name: &str,
    server: &ServerEntry,
    instances: Option<u8>,
    service: Option<String>,
    reset: bool,
) -> Result<ScaleResult, Box<dyn std::error::Error + Send + Sync>> {
    let _t = output::timed(&format!("Scale {app_name}"));
    let mut client = ManagementClient::for_server(server).await?;
    let response = client
        .send(&Command::Scale {
            app: app_name.to_string(),
            instances,
            service,
            reset,
        })
        .await?;

    match response {
        Response::Ok { data } => {
            let result = ScaleResult {
                stopped: data
                    .get("stopped")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                service: data
                    .get("service")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                instances: data
                    .get("instances")
                    .and_then(|value| value.as_u64())
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or("Scale response is missing the effective instance count")?,
                standby_limited: data
                    .get("standby_limited")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            };
            tracing::info!(
                service = result.service.as_deref(),
                instances = result.instances,
                standby_limited = result.standby_limited,
                "Scaled {app_name}"
            );
            Ok(result)
        }
        Response::Error { message, .. } => Err(message.into()),
    }
}
