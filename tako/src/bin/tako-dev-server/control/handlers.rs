mod apps;
mod queries;
mod subscriptions;

use std::sync::{Arc, Mutex};

use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::protocol::{RegisterAppRequest, Request, Response};
use tako_socket::{read_json_line, write_json_line};

use super::lan::handle_toggle_lan;
use super::state::State;
use crate::tunnel::handle_toggle_tunnel;

pub(crate) async fn handle_client(
    stream: UnixStream,
    state: Arc<Mutex<State>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    loop {
        let Some(req) = (match read_json_line::<_, Request>(&mut r).await {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                write_resp(
                    &mut w,
                    &Response::Error {
                        message: format!("invalid request: {}", e),
                    },
                )
                .await?;
                continue;
            }
            Err(e) => return Err(e.into()),
        }) else {
            break;
        };

        let resp = match req {
            Request::Ping => Response::Pong,
            Request::SubscribeEvents => {
                subscriptions::subscribe_events(&state, &mut r, &mut w).await?;
                return Ok(());
            }
            Request::SubscribeLogs { config_path, after } => {
                match subscriptions::subscribe_logs(&state, &mut r, &mut w, config_path, after)
                    .await?
                {
                    subscriptions::StreamDisposition::Continue => continue,
                    subscriptions::StreamDisposition::Done => return Ok(()),
                }
            }
            Request::RegisterApp(request) => {
                let RegisterAppRequest {
                    config_path,
                    project_dir,
                    app_name,
                    variant,
                    hosts,
                    command,
                    env,
                    secrets,
                    images,
                    storages,
                    client_pid,
                    readiness_failure_hint,
                    worker_command,
                } = *request;
                apps::register_app(
                    Arc::clone(&state),
                    apps::RegisterAppArgs {
                        config_path,
                        project_dir,
                        app_name,
                        variant,
                        hosts,
                        command,
                        env,
                        secrets,
                        images,
                        storages,
                        client_pid,
                        readiness_failure_hint,
                        worker_command,
                    },
                )
                .await?
            }
            Request::UnregisterApp { config_path } => apps::unregister_app(&state, config_path),
            Request::RestartApp { config_path } => apps::restart_app(&state, config_path),
            Request::SetAppStatus {
                config_path,
                status,
            } => match apps::set_app_status(&state, config_path, status) {
                Ok(resp) => resp,
                Err(resp) => {
                    write_resp(&mut w, &resp).await?;
                    continue;
                }
            },
            Request::HandoffApp { config_path, pid } => apps::handoff_app(&state, config_path, pid),
            Request::ConnectClient {
                config_path,
                client_id,
            } => {
                subscriptions::connect_client(&state, &mut r, &mut w, config_path, client_id)
                    .await?;
                return Ok(());
            }
            Request::ListRegisteredApps => queries::list_registered_apps(&state),
            Request::ListApps => queries::list_apps(&state),
            Request::Info => queries::info(&state),
            Request::ToggleLan { enabled } => handle_toggle_lan(&state, enabled).await,
            Request::ToggleTunnel {
                config_path,
                enabled,
            } => handle_toggle_tunnel(&state, config_path, enabled).await,
            Request::StopServer => queries::stop_server(&state),
        };

        write_resp(&mut w, &resp).await?;
    }

    Ok(())
}

async fn write_resp(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &Response,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    write_json_line(w, resp).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
