mod client;
pub mod close_codes;
pub mod error_codes;
pub mod params;
pub mod registry;
mod routing;
mod store;
mod types;

pub use client::{
    ChannelDispatchRequest, ChannelDispatchResponse, authorize_channel_request,
    dispatch_channel_message,
};
pub use close_codes::ChannelCloseCode;
pub use routing::{parse_channel_route, parse_message_id_cursor, parse_ws_last_message_id};
pub use store::{ChannelStore, channels_db_path};
pub use types::{
    ChannelAuthResponse, ChannelAuthScheme, ChannelAuthVerifyRequest, ChannelDefinitionMeta,
    ChannelError, ChannelHeaderValue, ChannelMessage, ChannelOperation, ChannelPublishPayload,
    ChannelRoute, ChannelTransport,
};

#[cfg(test)]
pub(crate) use types::{
    DEFAULT_KEEPALIVE_INTERVAL_MS, DEFAULT_MAX_CONNECTION_LIFETIME_MS, DEFAULT_REPLAY_WINDOW_MS,
};

pub const TAKO_PUBLIC_BASE_PATH: &str = "/_tako";
pub const CHANNELS_BASE_PATH: &str = "/_tako/channels/";
pub const INTERNAL_CHANNEL_AUTH_PATH: &str = "/channels/authorize";
pub const INTERNAL_CHANNEL_DISPATCH_PATH: &str = "/channels/dispatch";
pub const INTERNAL_CHANNEL_REGISTRY_PATH: &str = "/channels/registry";

#[cfg(test)]
mod tests;
