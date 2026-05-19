use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::protocol::{self, Response};

use super::super::state::{ControlClientSubscription, State};
use super::write_resp;

pub(super) enum StreamDisposition {
    Continue,
    Done,
}

pub(super) async fn subscribe_events(
    state: &Arc<Mutex<State>>,
    r: &mut BufReader<OwnedReadHalf>,
    w: &mut OwnedWriteHalf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rx = {
        let s = state.lock().unwrap();
        s.events.subscribe()
    };

    let _control_client = ControlClientSubscription::register(state);
    let mut rx = rx;
    if write_resp(w, &Response::Subscribed).await.is_err() {
        return Ok(());
    }
    let mut disconnect_probe = [0_u8; 1];
    loop {
        tokio::select! {
            maybe_resp = rx.recv() => {
                let Some(resp) = maybe_resp else {
                    break;
                };
                if write_resp(w, &resp).await.is_err() {
                    break;
                }
            }
            read_result = r.read(&mut disconnect_probe) => {
                match read_result {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn subscribe_logs(
    state: &Arc<Mutex<State>>,
    r: &mut BufReader<OwnedReadHalf>,
    w: &mut OwnedWriteHalf,
    config_path: String,
    after: Option<u64>,
) -> Result<StreamDisposition, Box<dyn std::error::Error + Send + Sync>> {
    let log_buffer = {
        let s = state.lock().unwrap();
        s.apps.get(&config_path).map(|a| a.log_buffer.clone())
    };

    let Some(log_buffer) = log_buffer else {
        write_resp(
            w,
            &Response::Error {
                message: format!("app not found: {config_path}"),
            },
        )
        .await?;
        return Ok(StreamDisposition::Continue);
    };

    let _control_client = ControlClientSubscription::register(state);
    let (backlog, mut rx, truncated) = log_buffer.subscribe(after);

    if write_resp(w, &Response::LogsSubscribed).await.is_err() {
        return Ok(StreamDisposition::Done);
    }
    if truncated && write_resp(w, &Response::LogsTruncated).await.is_err() {
        return Ok(StreamDisposition::Done);
    }

    for entry in backlog {
        if write_resp(
            w,
            &Response::LogEntry {
                id: entry.id,
                line: entry.line,
            },
        )
        .await
        .is_err()
        {
            return Ok(StreamDisposition::Done);
        }
    }

    let mut disconnect_probe = [0_u8; 1];
    loop {
        tokio::select! {
            maybe_entry = rx.recv() => {
                let Some(entry) = maybe_entry else {
                    break;
                };
                if write_resp(
                    w,
                    &Response::LogEntry {
                        id: entry.id,
                        line: entry.line,
                    },
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            read_result = r.read(&mut disconnect_probe) => {
                match read_result {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }
    Ok(StreamDisposition::Done)
}

pub(super) async fn connect_client(
    state: &Arc<Mutex<State>>,
    r: &mut BufReader<OwnedReadHalf>,
    w: &mut OwnedWriteHalf,
    config_path: String,
    client_id: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app_name = {
        let s = state.lock().unwrap();
        let name = s
            .apps
            .get(&config_path)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        s.events.broadcast(Response::Event {
            event: protocol::DevEvent::ClientConnected {
                config_path: config_path.clone(),
                app_name: name.clone(),
                client_id,
            },
        });
        name
    };

    if write_resp(w, &Response::Pong).await.is_err() {
        return Ok(());
    }

    let mut probe = [0_u8; 1];
    loop {
        match r.read(&mut probe).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }

    {
        let s = state.lock().unwrap();
        s.events.broadcast(Response::Event {
            event: protocol::DevEvent::ClientDisconnected {
                config_path,
                app_name,
                client_id,
            },
        });
    }

    Ok(())
}
