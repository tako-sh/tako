use super::*;

#[derive(Debug, Clone)]
pub(crate) struct TunnelRegistration {
    pub(super) generation: u64,
    pub(crate) url: String,
    pub(crate) expires_at: u64,
    pub(crate) abort_handle: AbortHandle,
}

#[derive(Default)]
pub(crate) struct TunnelLifecycle {
    pub(super) generation: u64,
    // Keep this gate across app re-registration so an old remote registration
    // cannot rotate the session token after a newer enable has completed.
    registration: Arc<tokio::sync::Mutex<()>>,
}

pub(crate) async fn handle_toggle_tunnel(
    state: &Arc<Mutex<State>>,
    config_path: String,
    enabled: bool,
) -> Response {
    if enabled {
        match enable_tunnel(state, &config_path).await {
            Ok((url, expires_at)) => Response::TunnelToggled {
                config_path,
                enabled: true,
                url: Some(url),
                expires_at: Some(expires_at),
            },
            Err(message) => Response::Error { message },
        }
    } else {
        let (url, expires_at) = disable_tunnel(state, &config_path, TunnelCloseReason::User);
        Response::TunnelToggled {
            config_path,
            enabled: false,
            url,
            expires_at,
        }
    }
}

pub(super) async fn enable_tunnel(
    state: &Arc<Mutex<State>>,
    config_path: &str,
) -> Result<(String, u64), String> {
    let (snapshot, app_instance, generation, registration) = {
        let mut s = state.lock().map_err(|_| "dev server state poisoned")?;
        let Some(app) = s.apps.get(config_path) else {
            return Err("app is not registered".to_string());
        };
        if let Some(tunnel) = &app.tunnel {
            return Ok((tunnel.url.clone(), tunnel.expires_at));
        }
        let Some(host) = primary_local_host(&app.hosts) else {
            return Err("app has no concrete local host".to_string());
        };
        let app_instance = app.bootstrap_token.clone();
        let snapshot = TunnelSnapshot {
            config_path: config_path.to_string(),
            app_name: app.name.clone(),
            local_host: host,
            listen_addr: s.listen_addr.clone(),
        };
        let lifecycle = s
            .tunnel_lifecycles
            .entry(config_path.to_string())
            .or_default();
        lifecycle.generation = lifecycle.generation.wrapping_add(1);
        (
            snapshot,
            app_instance,
            lifecycle.generation,
            lifecycle.registration.clone(),
        )
    };
    // Own registration to completion even if the control client disconnects.
    // The next enable waits until an earlier remote token rotation has finished.
    let state = state.clone();
    tokio::spawn(async move {
        let _registration = registration.lock().await;
        enable_tunnel_generation(state, snapshot, app_instance, generation).await
    })
    .await
    .map_err(|error| format!("tunnel registration failed: {error}"))?
}

pub(super) fn registration_is_current(
    state: &State,
    config_path: &str,
    app_instance: &str,
    generation: u64,
) -> bool {
    state
        .tunnel_lifecycles
        .get(config_path)
        .is_some_and(|lifecycle| lifecycle.generation == generation)
        && state
            .apps
            .get(config_path)
            .is_some_and(|app| app.bootstrap_token == app_instance)
}

pub(super) async fn enable_tunnel_generation(
    state: Arc<Mutex<State>>,
    snapshot: TunnelSnapshot,
    app_instance: String,
    generation: u64,
) -> Result<(String, u64), String> {
    {
        let s = state.lock().map_err(|_| "dev server state poisoned")?;
        if !registration_is_current(&s, &snapshot.config_path, &app_instance, generation) {
            return Err("tunnel enable was superseded".into());
        }
    }
    let base_url = tunnel_base_url();
    let identity = TakoIdentity::load_or_create()?;
    let public_key = identity.public_key()?;
    let challenge = create_tunnel_challenge(&base_url, &snapshot.app_name, &public_key).await?;
    let signature = identity.sign_tunnel(
        &challenge.nonce,
        &snapshot.app_name,
        &challenge.host,
        &public_key,
    )?;
    let created = create_tunnel(
        &base_url,
        &snapshot.app_name,
        &public_key,
        &challenge.nonce,
        &signature,
    )
    .await?;
    let connect_url = tunnel_connect_url(&base_url, &created.host, &created.session)?;

    let config_path = snapshot.config_path.clone();
    let state_for_task = state.clone();
    let config_for_task = config_path.clone();
    let url_for_task = created.url.clone();
    let (start_tx, start_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        run_tunnel_session(
            connect_url,
            snapshot,
            state_for_task,
            config_for_task,
            url_for_task,
            generation,
        )
        .await;
    });
    let abort_handle = task.abort_handle();
    let mut s = state.lock().map_err(|_| "dev server state poisoned")?;
    if !registration_is_current(&s, &config_path, &app_instance, generation) {
        abort_handle.abort();
        return Err("tunnel enable was superseded".into());
    }
    let app = s
        .apps
        .get_mut(&config_path)
        .expect("current registration has an app");
    let app_name = app.name.clone();
    app.tunnel = Some(TunnelRegistration {
        generation,
        url: created.url.clone(),
        expires_at: created.expires_at,
        abort_handle,
    });
    s.events.broadcast(Response::Event {
        event: protocol::DevEvent::TunnelModeChanged {
            config_path,
            app_name,
            enabled: true,
            url: Some(created.url.clone()),
            expires_at: Some(created.expires_at),
            close_reason: None,
        },
    });
    let _ = start_tx.send(());
    Ok((created.url, created.expires_at))
}

pub(super) fn disable_tunnel(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    reason: TunnelCloseReason,
) -> (Option<String>, Option<u64>) {
    disable_tunnel_generation(state, config_path, reason, None)
}

pub(super) fn disable_tunnel_generation(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    reason: TunnelCloseReason,
    expected: Option<u64>,
) -> (Option<String>, Option<u64>) {
    let mut s = match state.lock() {
        Ok(s) => s,
        Err(_) => return (None, None),
    };
    if let Some(expected) = expected
        && s.apps
            .get(config_path)
            .and_then(|app| app.tunnel.as_ref())
            .is_none_or(|tunnel| tunnel.generation != expected)
    {
        return (None, None);
    }
    let lifecycle = s
        .tunnel_lifecycles
        .entry(config_path.to_string())
        .or_default();
    lifecycle.generation = lifecycle.generation.wrapping_add(1);
    let Some(app) = s.apps.get_mut(config_path) else {
        return (None, None);
    };
    let Some(tunnel) = app.tunnel.take() else {
        return (None, None);
    };
    if matches!(
        reason,
        TunnelCloseReason::User | TunnelCloseReason::Shutdown
    ) {
        tunnel.abort_handle.abort();
    }
    let url = tunnel.url;
    let expires_at = tunnel.expires_at;
    let app_name = app.name.clone();
    s.events.broadcast(Response::Event {
        event: protocol::DevEvent::TunnelModeChanged {
            config_path: config_path.to_string(),
            app_name,
            enabled: false,
            url: None,
            expires_at: None,
            close_reason: Some(reason),
        },
    });
    (Some(url), Some(expires_at))
}

pub(super) fn tunnel_is_current(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    generation: u64,
) -> bool {
    let s = match state.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    s.apps
        .get(config_path)
        .and_then(|app| app.tunnel.as_ref())
        .is_some_and(|tunnel| tunnel.generation == generation)
}

pub(super) async fn current_upstream_port(
    state: &Arc<Mutex<State>>,
    config_path: &str,
) -> Result<u16, String> {
    let routes = {
        let s = state.lock().map_err(|_| "dev server state poisoned")?;
        if !s.apps.contains_key(config_path) {
            return Err("app is not registered".to_string());
        }
        s.routes.clone()
    };

    routes
        .wait_for_active_port(&format!("reg:{config_path}"), Duration::from_secs(30))
        .await
        .filter(|port| *port != 0)
        .ok_or_else(|| "app has no active upstream port".to_string())
}

pub(super) fn broadcast_tunnel_connection(
    state: &Arc<Mutex<State>>,
    config_path: &str,
    app_name: &str,
    url: &str,
    connected: bool,
    generation: u64,
) {
    let s = match state.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if s.apps
        .get(config_path)
        .and_then(|app| app.tunnel.as_ref())
        .is_none_or(|tunnel| tunnel.generation != generation)
    {
        return;
    }
    s.events.broadcast(Response::Event {
        event: protocol::DevEvent::TunnelConnectionChanged {
            config_path: config_path.to_string(),
            app_name: app_name.to_string(),
            connected,
            url: url.to_string(),
        },
    });
}

pub(super) fn primary_local_host(hosts: &[String]) -> Option<String> {
    hosts
        .iter()
        .map(|host| host.split('/').next().unwrap_or(host))
        .find(|host| !host.starts_with("*.") && !host.is_empty())
        .map(str::to_string)
}
