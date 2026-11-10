mod api;
mod handshake;
mod http;
mod lifecycle;
mod session;
mod websocket;

use api::*;
use http::*;
pub(super) use lifecycle::handle_toggle_tunnel;
use lifecycle::*;
pub(crate) use lifecycle::{TunnelLifecycle, TunnelRegistration};
use session::*;
use websocket::*;

use handshake::connect_tunnel_socket;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use crate::tunnel_wire::*;
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::CloseFrame},
};

use crate::protocol::{self, Response};

use crate::control::State;
use crate::identity::TakoIdentity;

const DEFAULT_TUNNEL_BASE_URL: &str = "https://tako.website/api";
const TUNNEL_API_TIMEOUT: Duration = Duration::from_secs(15);
const TUNNEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TUNNEL_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);
const TUNNEL_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
const TUNNEL_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
const TUNNEL_CLOSE_ACTIVE_LIMIT: u16 = 4409;
const TUNNEL_CLOSE_REPLACED: u16 = 4410;

type TunnelCloseReason = protocol::TunnelCloseReason;

#[derive(Clone)]
struct TunnelSnapshot {
    config_path: String,
    app_name: String,
    local_host: String,
    listen_addr: String,
}

fn is_hop_by_hop_or_host(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "tunnel_benchmark.rs"]
mod benchmark;

#[cfg(test)]
#[path = "tunnel_transport_tests.rs"]
mod transport_tests;

#[cfg(test)]
mod lifecycle_tests;
