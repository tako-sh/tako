//! Tako-hosted SQL databases.
//!
//! tako-server owns each database file and serves it to apps over the SQL over
//! HTTP protocol (Hrana over HTTP with JSON), so existing libSQL and Turso
//! serverless clients work unchanged. Apps on the database's server send their
//! binding token; apps on other servers reach it through their own server,
//! which forwards the request over the private network.

#[cfg(feature = "client")]
pub mod client;
mod engine;
mod exec;
mod host;
pub mod http;
pub mod migrate;
mod mvcc;
pub mod proto;
#[cfg(feature = "testing")]
pub mod testing;

pub use engine::{EngineConfig, Synchronous, replace_database, verify_database};
pub use host::{
    AdminQuery, Binding, DatabaseHost, DatabaseId, Home, HostConfig, HostError, PauseGuard,
    TokenHash, generate_token, hash_token,
};
pub use migrate::{Migration, MigrationReport};
pub use mvcc::{
    PortableDatabase, PortableValue, materialize_recovery_image, reset_database_file,
    verify_hosted_import,
};
pub use proto::Value;
