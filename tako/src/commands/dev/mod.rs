//! Tako Dev Client
//!
//! CLI client for the tako-dev-server daemon:
//! - HTTPS via local CA (`{app-name}.test` / `{app-name}.tako.test`)
//! - Local authoritative DNS for `*.test` and `*.tako.test`
//! - `tako.toml` watching for env/route updates
//! - Streaming logs, status, and resource monitoring
//! - Process lifecycle managed by the daemon

mod client;
mod output;
mod output_render;
pub(crate) mod prepare;
mod project;
mod runner;
mod shared;
mod types;
mod watcher;

#[cfg(test)]
use std::time::Duration;

use crate::app::resolve_app_name_from_config_path;
use crate::build::{PresetGroup, apply_adapter_base_runtime_defaults, js};
use crate::dev::LocalCA;
use client::{ConnectedDevClient, parse_log_line, run_connected_dev_client};
#[cfg(test)]
use prepare::local::tcp_probe;
use prepare::local::{local_https_probe_host, wait_for_https_host_reachable_via_ip};
#[cfg(target_os = "macos")]
use prepare::macos::ensure_local_dns_resolver_configured;
#[cfg(not(target_os = "macos"))]
fn ensure_local_dns_resolver_configured(_port: u16) -> Result<bool, Box<dyn std::error::Error>> {
    Ok(true)
}
#[cfg(all(test, target_os = "macos"))]
use prepare::macos::{
    local_dns_resolver_contents, local_dns_sudo_action_line, parse_local_dns_resolver,
    sudo_setup_action_items,
};
use prepare::tls::ensure_dev_server_tls_material;
#[cfg(test)]
use prepare::tls::{
    ca_fingerprint, ca_fingerprint_path_for_home, dev_server_tls_names_path_for_home,
    dev_server_tls_paths_for_home, ensure_dev_server_tls_material_for_home,
};
use project::{
    compute_dev_env, compute_dev_hosts, compute_display_routes, dev_startup_lines, dev_url,
    disambiguate_app_name, has_explicit_dev_preset, infer_preset_name_from_ref,
    inject_dev_allowed_hosts, inject_dev_data_dir, inject_dev_secrets, preferred_public_url,
    readiness_failure_hint_for_dev_command, resolve_dev_preset_ref, resolve_dev_run_command,
    resolve_dev_worker_command, resolve_effective_dev_build_adapter, try_list_registered_app_names,
};
#[cfg(test)]
use project::{
    dev_runtime_data_root, route_hostname_matches, sanitize_name_segment, short_path_hash,
};
#[cfg(test)]
use shared::{doctor_dev_server_lines, doctor_local_forwarding_preflight_lines};
pub use types::{DevEvent, LogLevel, ScopedLog};
#[cfg(test)]
use types::{
    app_log_scope, child_log_level_and_message, should_drop_child_log_line, trim_child_log_message,
};

#[cfg(target_os = "linux")]
pub(crate) use prepare::linux::{LinuxSetupStatus, status as linux_setup_status};
pub(crate) use prepare::local::is_dev_server_unavailable_error_message;
#[cfg(target_os = "macos")]
pub(crate) use prepare::macos::local_dns_resolver_values;
#[cfg(target_os = "macos")]
pub(crate) use prepare::macos::{DevProxyStatus, status as dev_proxy_status};
pub use prepare::tls::setup_local_ca;

#[cfg(test)]
const DEV_INITIAL_INSTANCE_COUNT: usize = 1;
#[cfg(test)]
const DEV_IDLE_TIMEOUT_SECS: u64 = 30 * 60;
pub(crate) const LOCAL_DNS_PORT: u16 = 53535;
#[cfg(target_os = "macos")]
const RESOLVER_DIR: &str = "/etc/resolver";
#[cfg(target_os = "macos")]
pub(crate) const TAKO_RESOLVER_FILE: &str = "/etc/resolver/tako.test";
#[cfg(target_os = "macos")]
pub(crate) const SHORT_RESOLVER_FILE: &str = "/etc/resolver/test";
const LOCALHOST_443_HTTPS_PROBE_ATTEMPTS: usize = 12;
const LOCALHOST_443_HTTPS_PROBE_TIMEOUT_MS: u64 = 500;
const LOCALHOST_443_HTTPS_PROBE_RETRY_DELAY_MS: u64 = 150;
pub(crate) const DEV_LOOPBACK_ADDR: &str = "127.77.0.1";

#[cfg(test)]
fn dev_initial_instance_count() -> usize {
    DEV_INITIAL_INSTANCE_COUNT
}

#[cfg(test)]
fn dev_idle_timeout() -> Duration {
    Duration::from_secs(DEV_IDLE_TIMEOUT_SECS)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use shared::system_resolver_ipv4;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use shared::{
    load_dev_tako_toml, port_from_listen, restart_required_for_requested_listen,
};

pub use runner::{ls, run, stop};
#[cfg(test)]
mod tests;
