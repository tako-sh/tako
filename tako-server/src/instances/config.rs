use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::{LEGACY_INTERNAL_STATUS_HOST, internal_app_host};

/// Configuration for an app
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// App name
    pub name: String,
    /// Deployment environment
    pub environment: String,
    /// Current version
    pub version: String,
    /// Derived path to the active app directory
    pub path: PathBuf,
    /// Runtime command derived from app.json
    pub command: Vec<String>,
    /// How instances for this release are launched.
    pub launch: AppLaunch,
    /// Non-secret environment variables (read from app.json in release dir)
    pub env_vars: HashMap<String, String>,
    /// Secret environment variables (loaded from encrypted server state)
    pub secrets: HashMap<String, String>,
    /// Storage bindings loaded from encrypted server state.
    pub storages: HashMap<String, tako_core::StorageBinding>,
    /// Client source-IP mode for requests routed to this app.
    pub source_ip: tako_core::SourceIpMode,
    /// Public image optimizer configuration from app.json.
    pub images: tako_images::ImagesConfig,
    /// Minimum instances (0 = on-demand)
    pub min_instances: u32,
    /// Maximum instances
    pub max_instances: u32,
    /// Health check path
    pub health_check_path: String,
    /// Health check host header
    pub health_check_host: String,
    /// Startup timeout
    pub startup_timeout: Duration,
    /// Idle timeout (for on-demand scaling)
    pub idle_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppLaunch {
    Native,
    Container { image: String, port: u16 },
}

impl AppConfig {
    pub fn deployment_id(&self) -> String {
        if self.environment.is_empty() {
            return self.name.clone();
        }
        tako_core::deployment_app_id(&self.name, &self.environment)
    }

    fn internal_host(&self) -> String {
        internal_app_host(&self.name)
    }

    pub(super) fn apply_internal_defaults(&mut self) {
        if self.health_check_host.is_empty()
            || self.health_check_host == LEGACY_INTERNAL_STATUS_HOST
        {
            self.health_check_host = self.internal_host();
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            environment: String::new(),
            version: String::new(),
            path: PathBuf::new(),
            command: vec![],
            launch: AppLaunch::Native,
            env_vars: HashMap::new(),
            secrets: HashMap::new(),
            storages: HashMap::new(),
            source_ip: tako_core::SourceIpMode::Auto,
            images: tako_images::ImagesConfig::default(),
            min_instances: 1,
            max_instances: 4,
            health_check_path: "/status".to_string(),
            health_check_host: LEGACY_INTERNAL_STATUS_HOST.to_string(),
            startup_timeout: Duration::from_secs(30),
            idle_timeout: crate::defaults::DEFAULT_IDLE_TIMEOUT,
        }
    }
}

pub(crate) fn validate_requested_instances(
    requested_instances: u32,
    max_instances: u32,
) -> Result<(), String> {
    let limit = effective_instance_limit(max_instances);
    if requested_instances > limit {
        return Err(format!(
            "Requested {requested_instances} instances, but this server allows at most {limit}. Use fewer instances or spread traffic across more servers."
        ));
    }
    Ok(())
}

pub(crate) fn default_max_instances_for_host() -> u32 {
    host_instance_limit_for_parallelism(host_parallelism())
}

pub(crate) fn effective_instance_limit(max_instances: u32) -> u32 {
    max_instances.min(default_max_instances_for_host())
}

pub(crate) fn clamp_instances_to_limit(config: &mut AppConfig) -> Option<(u32, u32)> {
    let limit = effective_instance_limit(config.max_instances);
    if config.min_instances <= limit {
        return None;
    }

    let requested = config.min_instances;
    config.min_instances = limit;
    Some((requested, limit))
}

fn host_parallelism() -> u32 {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .try_into()
        .unwrap_or(u32::MAX)
}

pub(crate) fn host_instance_limit_for_parallelism(parallelism: u32) -> u32 {
    parallelism.saturating_mul(2).max(1)
}
