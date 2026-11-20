use std::path::Path;

use crate::{config::TakoToml, management_http::ManagementClient, output};
use tako_core::{Command, Response};

pub fn run(
    running: bool,
    service: Option<&str>,
    env: Option<&str>,
    server: Option<&str>,
    app: Option<&str>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let context = super::project_context::resolve_optional(config_path)?;
        let config = context
            .as_ref()
            .map(|c| TakoToml::load_from_file(&c.config_path))
            .transpose()?;
        let target = super::app_target::resolve(
            config.as_ref(),
            context.as_ref().map(|c| c.config_path.as_path()),
            app,
            env,
            server,
        )?;
        let servers = super::env_network::servers_for_target(
            context.as_ref().map(|c| c.project_dir.as_path()),
            config.as_ref(),
            target.env_name.as_deref(),
        )
        .await?;
        let names = super::app_target::server_names(
            config.as_ref(),
            &servers,
            target.env_name.as_deref(),
            server,
        )?;
        let verb = if running { "Start" } else { "Stop" };
        let past = if running { "Started" } else { "Stopped" };
        let label = match service {
            Some(service) => format!("{} / {service}", target.remote_app_name),
            None => target.remote_app_name.clone(),
        };
        output::section(verb);
        let command = if running {
            Command::Start {
                app: target.remote_app_name,
                service: service.map(str::to_owned),
            }
        } else {
            Command::Stop {
                app: target.remote_app_name,
                service: service.map(str::to_owned),
            }
        };
        let mut failures = Vec::new();
        for name in names {
            if output::is_dry_run() {
                output::dry_run_skip(&format!("{verb} {label} on {name}"));
                continue;
            }
            let _timer = output::timed(&format!("[{name}] {verb} {label}"));
            let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
                let entry = servers
                    .get(&name)
                    .ok_or("Server missing from enrolled network membership")?;
                let mut client = ManagementClient::for_server(entry).await?;
                match client.send(&command).await? {
                    Response::Ok { data } => match data.get("cleanup_error").and_then(|v| v.as_str()) {
                        Some(error) => Err(format!("Runtime state saved, but container cleanup failed: {error}. Retry the command.").into()),
                        None => Ok(()),
                    },
                    Response::Error { message, .. } => Err(message.into()),
                }
            }
            .await;
            match result {
                Ok(()) => output::success(&format!("{past} {label} on {name}")),
                Err(error) => {
                    output::error(&format!("{name}: {error}"));
                    failures.push(name);
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!("{verb} failed on {}", failures.join(", ")).into())
        }
    })
}
