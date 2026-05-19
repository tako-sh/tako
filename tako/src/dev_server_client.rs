mod apps;
mod commands;
mod connection;
mod daemon;
mod events;

#[allow(unused_imports)]
pub use apps::{
    ListedApp, RegisterAppRequest, RegisteredAppInfo, connect_client, list_apps,
    list_registered_apps, register_app, restart_app, unregister_app,
};
pub use commands::{info, stop_server, toggle_lan};
#[allow(unused_imports)]
pub(crate) use connection::LineClient;
pub use daemon::ensure_running;
pub use events::{DevServerEvent, LogStreamEntry, subscribe_events, subscribe_logs};

#[cfg(test)]
mod tests;
