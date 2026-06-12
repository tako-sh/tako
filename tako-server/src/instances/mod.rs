//! Instance lifecycle management
//!
//! Manages app instances - spawning, health checking, and cleanup.

mod app;
mod config;
mod error;
mod health;
mod instance;
pub mod logger;
mod manager;
mod network;
mod rolling;
mod spawner;

#[cfg(test)]
pub(crate) use crate::socket::AppState;
pub(crate) use crate::socket::InstanceState;
pub use app::{App, InstanceEvent};
#[cfg(test)]
pub(crate) use config::effective_instance_limit;
#[cfg(test)]
pub(crate) use config::host_instance_limit_for_parallelism;
pub(crate) use config::{
    AppConfig, AppLaunch, clamp_instances_to_limit, default_max_instances_for_host,
    validate_requested_instances,
};
pub use error::InstanceError;
pub use health::*;
#[allow(unused_imports)]
pub(crate) use instance::HealthyInstance;
pub use instance::Instance;
pub use logger::{
    AppLogHandle, LogStream, app_log_tracing_layer, log_pipe, register_app_logger,
    spawn_app_logger, unregister_app_logger,
};
pub use manager::AppManager;
pub use network::*;
pub use rolling::*;
pub use spawner::*;

pub const INTERNAL_HOST_SUFFIX: &str = ".tako";
const LEGACY_INTERNAL_STATUS_HOST: &str = "tako.internal";
pub const INTERNAL_TOKEN_HEADER: &str = "X-Tako-Internal-Token";

pub fn internal_app_host(app_name: &str) -> String {
    let app_name = app_name.trim();
    let app_name = if app_name.is_empty() { "app" } else { app_name };
    format!("{app_name}{INTERNAL_HOST_SUFFIX}")
}

pub fn internal_app_host_for_app_id(app_id: &str) -> String {
    let app_name = tako_core::split_deployment_app_id(app_id)
        .map(|(name, _)| name)
        .unwrap_or(app_id);
    internal_app_host(app_name)
}

#[cfg(test)]
mod tests;
