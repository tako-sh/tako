mod client;
mod cursor;
mod mesh;
pub use cursor::ReplayCursor;
pub use mesh::{ChannelEvent, ChannelEventId, ChannelPublication, RemoteIngest};
pub mod ephemeral;
pub mod live_store;
mod presence;
pub use presence::{PRESENCE_CHANNEL_PREFIX, PresenceMember, PresenceSession, presence_scope};
pub mod close_codes;
pub mod error_codes;
pub mod params;
mod publication;
pub use publication::{TrustedPublication, validate_publish_params};
pub mod registry;
mod routing;
pub mod sse;
mod store;
mod stream;
pub use stream::{ChannelStream, STREAM_CHANNEL_PREFIX, reserved_stream_name};
mod types;
mod writer;
pub use writer::{ChannelWriter, LocalChannelWriter};
pub mod websocket;

pub use client::authorize_channel_request;
pub use close_codes::ChannelCloseCode;
pub use routing::{parse_channel_route, parse_message_id_cursor, parse_ws_last_message_id};
pub use store::{
    ChannelStore, ChannelStoreConfig, POSTGRES_CHANNELS_SCHEMA, SqliteReplayLimits,
    channels_db_path,
};
pub use types::{
    ChannelAuthResponse, ChannelAuthScheme, ChannelAuthVerifyRequest, ChannelDefinitionMeta,
    ChannelError, ChannelHeaderValue, ChannelLifecycle, ChannelMessage, ChannelOperation,
    ChannelPublishPayload, ChannelRoute, ChannelTransport,
};

#[cfg(test)]
pub(crate) use types::{
    DEFAULT_KEEPALIVE_INTERVAL_MS, DEFAULT_MAX_CONNECTION_LIFETIME_MS, DEFAULT_REPLAY_WINDOW_MS,
};

pub const TAKO_PUBLIC_BASE_PATH: &str = "/_tako";
pub const CHANNELS_BASE_PATH: &str = "/_tako/channels/";
pub const INTERNAL_CHANNEL_AUTH_PATH: &str = "/channels/authorize";
pub const INTERNAL_CHANNEL_REGISTRY_PATH: &str = "/channels/registry";

#[cfg(test)]
mod tests;

#[cfg(test)]
mod presence_tests;

#[cfg(test)]
mod replay_tests;

#[cfg(test)]
mod mesh_tests;

#[cfg(test)]
mod history_tests;

#[cfg(test)]
mod stream_tests;
