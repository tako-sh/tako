use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::process::push_scoped_log;
use crate::protocol::{self, Response};
use crate::route_pattern::split_route_pattern;
use crate::state;

use super::state::State;

pub(super) async fn handle_toggle_lan(state: &Arc<Mutex<State>>, enabled: bool) -> Response {
    if enabled {
        let lan_ip = match crate::lan::detect_lan_ip() {
            Some(ip) => ip,
            None => {
                return Response::Error {
                    message: "could not detect LAN IP address".to_string(),
                };
            }
        };

        // Snapshot log buffers ahead of the await so we don't hold the state
        // lock across it.
        let log_buffers: Vec<state::LogBuffer> = {
            let s = state.lock().unwrap();
            s.apps.values().map(|app| app.log_buffer.clone()).collect()
        };

        // If the first bind attempt in the dev proxy succeeds, enable_lan
        // returns in ~5-20ms and the user never sees a progress line. The
        // retry loop only kicks in after a 100ms backoff on EADDRINUSE, so an
        // 80ms delayed "Starting LAN mode…" log fires only when we hit the
        // retry path and have a real 100ms+ wait to explain.
        let progress_buffers = log_buffers.clone();
        let progress_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            for buffer in &progress_buffers {
                push_scoped_log(buffer, "Info", "tako", "Starting LAN mode…");
            }
        });

        // Bind the concrete LAN interface so the wildcard dev proxy listener on
        // loopback does not conflict with LAN exposure on macOS.
        let command = build_enable_lan_command(&lan_ip);
        let result = send_dev_proxy_command(&command).await;
        progress_task.abort();
        if let Err(e) = result {
            return Response::Error {
                message: format!("failed to enable LAN on dev proxy: {e}"),
            };
        }

        let ca_url = format!("http://{lan_ip}/ca.pem");

        let mut s = state.lock().unwrap();

        // Start mDNS publisher and publish all registered app hostnames
        let mut mdns = crate::lan::MdnsPublisher::new(lan_ip.clone());
        for app in s.apps.values() {
            for host in &app.hosts {
                mdns.publish(split_route_pattern(host).0);
            }
        }
        s.mdns = Some(mdns);
        s.lan_enabled = true;
        s.lan_ip = Some(lan_ip.clone());
        s.events.broadcast(Response::Event {
            event: protocol::DevEvent::LanModeChanged {
                enabled: true,
                lan_ip: Some(lan_ip.clone()),
                ca_url: Some(ca_url.clone()),
            },
        });
        Response::LanToggled {
            enabled: true,
            lan_ip: Some(lan_ip),
            ca_url: Some(ca_url),
        }
    } else {
        let _ = send_dev_proxy_command(r#"{"command":"disable_lan"}"#).await;

        let mut s = state.lock().unwrap();
        if let Some(ref mut mdns) = s.mdns {
            mdns.cleanup_all();
        }
        s.mdns = None;
        s.lan_enabled = false;
        s.lan_ip = None;
        s.events.broadcast(Response::Event {
            event: protocol::DevEvent::LanModeChanged {
                enabled: false,
                lan_ip: None,
                ca_url: None,
            },
        });
        Response::LanToggled {
            enabled: false,
            lan_ip: None,
            ca_url: None,
        }
    }
}

/// Send a command to the dev proxy control socket and read the response.
async fn send_dev_proxy_command(json_line: &str) -> Result<String, String> {
    const SOCKET_PATH: &str = "/tmp/tako-dev-proxy.sock";

    let stream = tokio::net::UnixStream::connect(SOCKET_PATH)
        .await
        .map_err(|e| format!("dev proxy not reachable at {SOCKET_PATH}: {e}"))?;

    let (reader, mut writer) = stream.into_split();
    let mut line = json_line.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    tokio::io::AsyncWriteExt::write_all(&mut writer, line.as_bytes())
        .await
        .map_err(|e| format!("failed to send command to dev proxy: {e}"))?;

    let mut reader = tokio::io::BufReader::new(reader);
    let mut response = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut response)
        .await
        .map_err(|e| format!("failed to read dev proxy response: {e}"))?;

    if response.contains("\"error\"") {
        return Err(response.trim().to_string());
    }
    Ok(response)
}

fn build_enable_lan_command(lan_ip: &str) -> String {
    serde_json::json!({
        "command": "enable_lan",
        "bind_addr": lan_ip,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::build_enable_lan_command;

    #[test]
    fn build_enable_lan_command_uses_detected_lan_ip() {
        let json = build_enable_lan_command("192.168.1.42");
        assert_eq!(
            json,
            r#"{"bind_addr":"192.168.1.42","command":"enable_lan"}"#
        );
    }
}
