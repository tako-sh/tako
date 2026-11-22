//! Tako-hosted key-value stores that speak the Redis protocol.
//!
//! A store is one redb file owned by the server that hosts it. Apps reach it
//! with any Redis client, authenticating with their binding token; apps on
//! other servers of the network reach it through their own server, which
//! relays the connection over the private network. Strings, counters, key
//! expiry, hashes and transactions (`MULTI`/`EXEC`/`WATCH`) are supported.

mod command;
mod glob;
mod host;
mod resp;
mod server;
mod store;

pub use host::{
    Binding, Home, HostConfig, HostError, KvHost, StoreId, TokenHash, generate_token, hash_token,
};
pub use resp::{Reply, encode_request};
pub use server::{Access, FORWARD_COMMAND, serve};
pub use store::{Clock, Stats, StoreError, system_clock};
