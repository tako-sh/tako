mod bootstrap;
mod control;
mod dev_channels;
mod identity;
mod image;
mod lan;
mod local_dns;
mod paths;
mod process;
mod protocol;
mod proxy;
mod redirect;
mod route_pattern;
mod state;
mod tls_accept;
mod tunnel;

use std::sync::{Arc, Mutex};

use pingora_core::listeners::TlsAccept;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::prelude::Server;
use std::path::PathBuf;
#[cfg(test)]
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::sync::watch;

use control::{EventsHub, State, handle_client};
use process::{handle_wake_on_request, kill_all_app_processes, stale_app_cleanup_loop};
use redirect::start_http_redirect_server;
use tls_accept::{DevCertResolver, load_or_create_ca};

use bootstrap::{
    HTTP_REDIRECT_LISTEN_ADDR, LOCAL_DNS_LISTEN_ADDR, acquire_pid_lock, default_socket_path,
    listen_port_from_addr, parse_args,
};
pub(crate) use bootstrap::{
    advertised_https_port, app_short_host, default_hosts, ensure_tcp_listener_can_bind,
};
use protocol::DevEvent;
use protocol::Response;
use tracing_subscriber::EnvFilter;
/// Split a route pattern like "app.test/api" into ("app.test", Some("/api")).
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        // Default to quiet. `RUST_LOG` can be used to enable info/debug output.
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let args = parse_args();

    // Acquire an exclusive PID lock. If another instance is running, SIGTERM it.
    let pid_path = paths::tako_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("dev-server.pid");
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _pid_lock = acquire_pid_lock(&pid_path)?;

    // Shared route table between the unix-socket control plane and the proxy.
    let routes = proxy::Routes::default();
    let events = EventsHub::default();

    // Events channel from Pingora runtime -> control-plane subscribers.
    // Also triggers wake-on-request for idle registered apps.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<DevEvent>();

    // Shared channel store — used both by the proxy (SSE subscribe + HTTP
    // publish from the outside) and by the internal socket's
    // `Command::ChannelPublish` handler (server-side publish from app/workflow
    // code). Same store so both paths see the same messages.
    let channels = dev_channels::DevChannelStore::new();
    // Start the Pingora proxy in a dedicated thread.
    // We exit the whole process when the control-plane tells us to shut down.
    {
        let listen = args.listen_addr.clone();
        ensure_tcp_listener_can_bind(&listen)?;

        let proxy = proxy::DevProxy {
            routes: routes.clone(),
            events: ev_tx.clone(),
            channels: channels.clone(),
        };

        // Workflow manager setup happens below, outside this block, so
        // registration handlers and the app-spawn path can both use it.

        let mut server = Server::new(None)?;
        server.bootstrap();
        let mut svc = pingora_proxy::http_proxy_service(&server.configuration, proxy);

        if let Some(app) = svc.app_logic_mut() {
            let mut opts = pingora_core::apps::HttpServerOptions::default();
            opts.keepalive_request_limit = Some(4096);
            app.server_options = Some(opts);
        }

        // Dynamic per-SNI cert generation: OpenSSL rejects `*.tako` wildcards
        // (single-label TLD), so we generate a cert per hostname on the fly.
        let ca = load_or_create_ca()?;
        let resolver = DevCertResolver::new(ca);
        let callbacks: Box<dyn TlsAccept + Send + Sync> = Box::new(resolver);
        // Keep dev HTTPS on HTTP/1.1. The dev E2E path holds an SSE response
        // open while issuing app requests through the same local proxy; the dev
        // proxy does not need h2, and production enables it separately.
        let tls = TlsSettings::with_callbacks(callbacks)?;
        svc.add_tls_with_settings(&listen, None, tls);

        server.add_service(svc);

        std::thread::spawn(move || {
            server.run_forever();
        });
    }
    let listen_addr = args.listen_addr;
    let listen_port = listen_port_from_addr(&listen_addr);

    let loopback_ip = args.dns_ip.parse::<std::net::Ipv4Addr>()?;
    let local_dns = local_dns::start(LOCAL_DNS_LISTEN_ADDR, loopback_ip).await?;
    tracing::info!(listen = %local_dns.listen_addr(), "local DNS server listening");

    let sock = default_socket_path();
    if let Some(parent) = sock.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Avoid clobbering an existing running dev server.
    // Unconditionally unlinking the socket is dangerous: a second instance can remove the socket
    // file, breaking clients of the first instance, and then fail to start.
    if tokio::fs::try_exists(&sock).await.unwrap_or(false) {
        match tokio::net::UnixStream::connect(&sock).await {
            Ok(_) => {
                return Err(format!(
                    "dev server already running (socket exists at {})",
                    sock.display()
                )
                .into());
            }
            Err(e) => {
                // If the socket file exists but nothing is listening, remove it.
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::NotConnected
                        | std::io::ErrorKind::ConnectionReset
                ) {
                    let _ = tokio::fs::remove_file(&sock).await;
                }
            }
        }
    }
    let listener = UnixListener::bind(&sock)?;
    tracing::info!(sock = %sock.display(), "tako-dev-server listening");

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let ca_pem = paths::tako_data_dir()
        .ok()
        .and_then(|dir| std::fs::read(dir.join("ca").join("ca.crt")).ok());
    start_http_redirect_server(HTTP_REDIRECT_LISTEN_ADDR, shutdown_rx.clone(), ca_pem).await?;
    tracing::info!(listen = %HTTP_REDIRECT_LISTEN_ADDR, "http redirect server listening");
    // Bring up the internal socket early (shared for workflows + channels),
    // so app registrations can call DevWorkflows::ensure() immediately.
    let data_dir = paths::tako_data_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workflows = Arc::new(tako_workflows::WorkflowManager::new(&data_dir));

    // Server-side channel `.publish()` — goes through the internal
    // socket straight to the channel store, no HTTPS/auth roundtrip.
    {
        let channels = channels.clone();
        workflows.set_channel_publisher(std::sync::Arc::new(
            move |app: &str, channel: &str, payload: serde_json::Value| {
                let typed: tako_channels::ChannelPublishPayload =
                    serde_json::from_value(payload).map_err(|e| format!("invalid payload: {e}"))?;
                channels
                    .publish(app, channel, &typed)
                    .map(|msg| serde_json::to_value(msg).unwrap_or(serde_json::Value::Null))
                    .map_err(|e| e.to_string())
            },
        ));
    }

    let internal_socket_path = match workflows.start_socket() {
        Err(e) => {
            tracing::warn!(error = %e, "failed to start internal socket; workflow enqueue / channel publish will not work");
            None
        }
        Ok(()) => {
            let path = workflows.socket_path();
            tracing::info!(socket = %path.display(), "internal socket listening");
            Some(path)
        }
    };

    let events_for_relay = events.clone();
    let mut st = State::new(
        shutdown_tx,
        routes,
        events,
        true,
        local_dns.port(),
        listen_port,
        listen_addr,
        args.dns_ip,
    );
    st.internal_socket = internal_socket_path;
    st.workflows = Some(workflows.clone());

    // Open the SQLite state store (persistent registrations only; runtime state is in-memory).
    let db_path = paths::tako_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("dev-server.sqlite");
    match state::DevStateStore::open(db_path) {
        Ok(db) => {
            // Kill any orphaned app processes from a previous (crashed) server run.
            if let Ok(apps) = db.list() {
                for app in &apps {
                    state::kill_orphaned_process(&app.project_dir, &app.config_path);
                }
            }
            st.db = Some(db);
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to open dev state store; registration disabled");
        }
    }

    let state = Arc::new(Mutex::new(st));

    // Events relay: broadcast to subscribers + trigger wake-on-request.
    {
        let events = events_for_relay;
        let state = state.clone();
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                if let DevEvent::RequestStarted { ref host, ref path } = ev {
                    let state = state.clone();
                    let host = host.clone();
                    let path = path.clone();
                    tokio::spawn(async move {
                        handle_wake_on_request(state, host, path).await;
                    });
                }
                events.broadcast(Response::Event { event: ev });
            }
        });
    }

    // Stale app cleanup loop.
    {
        let state = state.clone();
        tokio::spawn(async move { stale_app_cleanup_loop(state).await });
    }

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("tako-dev-server shutting down");
                    kill_all_app_processes(&state);
                    workflows.shutdown_all(std::time::Duration::from_secs(1)).await;
                    let _ = std::fs::remove_file(&sock);
                    std::process::exit(0);
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state).await {
                        tracing::warn!(err = %e, "client handler error");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
