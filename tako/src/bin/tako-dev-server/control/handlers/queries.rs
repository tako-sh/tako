use std::sync::{Arc, Mutex};

use crate::advertised_https_port;
use crate::protocol::{self, AppInfo, Response};

use super::super::state::State;

pub(super) fn list_registered_apps(state: &Arc<Mutex<State>>) -> Response {
    let s = state.lock().unwrap();
    let apps = s
        .apps
        .iter()
        .map(|(config_path, a)| {
            let (tunnel_url, tunnel_expires_at) = tunnel_info(a);
            protocol::RegisteredAppInfo {
                config_path: config_path.clone(),
                project_dir: a.project_dir.clone(),
                app_name: a.name.clone(),
                variant: a.variant.clone(),
                hosts: a.hosts.clone(),
                upstream_port: a.upstream_port,
                status: if a.is_idle { "idle" } else { "running" }.to_string(),
                pid: a.pid,
                client_pid: a.client_pid,
                tunnel_url,
                tunnel_expires_at,
            }
        })
        .collect();
    Response::RegisteredApps { apps }
}

pub(super) fn list_apps(state: &Arc<Mutex<State>>) -> Response {
    let s = state.lock().unwrap();
    let apps = s
        .apps
        .values()
        .map(|a| {
            let (tunnel_url, tunnel_expires_at) = tunnel_info(a);
            AppInfo {
                app_name: a.name.clone(),
                variant: a.variant.clone(),
                hosts: a.hosts.clone(),
                upstream_port: a.upstream_port,
                pid: a.pid,
                tunnel_url,
                tunnel_expires_at,
            }
        })
        .collect();
    Response::Apps { apps }
}

fn tunnel_info(app: &crate::state::RuntimeApp) -> (Option<String>, Option<u64>) {
    app.tunnel
        .as_ref()
        .map(|tunnel| (Some(tunnel.url.clone()), Some(tunnel.expires_at)))
        .unwrap_or((None, None))
}

pub(super) fn info(state: &Arc<Mutex<State>>) -> Response {
    let s = state.lock().unwrap();
    Response::Info {
        info: protocol::DevInfo {
            listen: s.listen_addr.clone(),
            port: advertised_https_port(&s),
            advertised_ip: s.advertised_ip.clone(),
            local_dns_enabled: s.local_dns_enabled,
            local_dns_port: s.local_dns_port,
            control_clients: s.control_clients,
            lan_enabled: s.lan_enabled,
            lan_ip: s.lan_ip.clone(),
        },
    }
}

pub(super) fn stop_server(state: &Arc<Mutex<State>>) -> Response {
    let s = state.lock().unwrap();
    let _ = s.shutdown_tx.send(true);
    Response::Stopping
}
