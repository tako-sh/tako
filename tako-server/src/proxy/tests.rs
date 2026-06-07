use super::request::{
    ClientIpResolution, ForwardedHeaderTrust, client_ip_for_source_ip_mode,
    client_ip_from_trusted_headers, forwarded_header_has_proto, forwarded_header_proto_is_https,
    https_redirect_host, ip_header_value, is_effective_request_https, is_request_forwarded_https,
    path_uses_tako_handler, strip_route_prefix_for_static_lookup, x_forwarded_proto_is_https,
};
use super::server::{create_tls_settings, listener_socket_options};
use super::*;
use crate::instances::{AppConfig, AppManager};
use crate::scaling::ColdStartConfig;
use crate::socket::InstanceState;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

mod backend_resolution;
mod cache;
mod core;
mod redirects;
mod responses;
mod source_ip;
mod static_assets;
