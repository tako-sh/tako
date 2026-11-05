//! Native network sidecar. The proxy must only use `relay`; it must never open
//! the controller or its nonce database. Parent startup wires `run` separately.
#![cfg(target_os = "linux")]

#[path = "native_agent/controller.rs"]
mod controller;
#[path = "native_agent/ipc.rs"]
mod ipc;
#[path = "native_agent/lifecycle.rs"]
mod lifecycle;
#[path = "native_agent/process.rs"]
mod process;
#[path = "native_agent/projection.rs"]
mod projection;
#[path = "native_agent/services.rs"]
mod services;
#[path = "native_agent/store.rs"]
mod store;

pub use ipc::{relay, run};
use serde::{Deserialize, Serialize};
use tako_network::{
    BootId,
    api::{RpcResponse, SignedRequest},
};

pub const MAX_REQUEST_BYTES: usize = tako_network::api::MAX_RPC_BYTES + 1024;
pub const MAX_RESPONSE_BYTES: usize = tako_network::corrosion::MAX_REGISTRY_BYTES + 1024 * 1024;
const MAX_HELPER_BYTES: usize = 1024 * 1024;
const QUEUE_SIZE: usize = 16;
const CONNECTIONS: usize = 32;

#[derive(thiserror::Error, Debug)]
pub enum AgentError {
    #[error("native agent I/O failed")]
    Io(#[from] std::io::Error),
    #[error("native agent configuration or identity failed validation")]
    Domain(#[from] tako_network::Error),
    #[error("native agent registry operation failed")]
    Runtime(#[from] tako_network::api::RuntimeError),
    #[error("native agent JSON failed validation")]
    Json(#[from] serde_json::Error),
    #[error("native agent: {0}")]
    Invalid(&'static str),
    #[error("native agent operation timed out; outcome is not acknowledged")]
    Timeout,
    #[error("Corrosion exceeded graceful shutdown deadline; forced termination was required")]
    ForcedShutdown,
    #[error("owned child could not be reaped; replacement is forbidden")]
    UnreapedChild,
}
type Result<T> = std::result::Result<T, AgentError>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRequest {
    pub method: String,
    pub path: String,
    pub signed: SignedRequest,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentResponse {
    pub status: u16,
    pub response: Option<RpcResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub boot: BootId,
    pub ready: bool,
}
impl AgentResponse {
    fn unavailable(boot: &BootId) -> Self {
        Self {
            status: 503,
            response: None,
            error: Some("native network is not ready".into()),
            boot: boot.clone(),
            ready: false,
        }
    }
}
fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| AgentError::Invalid("clock predates Unix epoch"))?
        .as_secs())
}

#[cfg(test)]
#[path = "native_agent/tests.rs"]
mod tests;
