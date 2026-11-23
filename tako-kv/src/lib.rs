//! Tako-hosted key-value stores: JSON values by key, with an optional TTL.
//!
//! A store is one redb file owned by the server that hosts it, and every
//! app environment that binds it sees the same keys. Apps call through
//! their server's internal socket with a binding token; a store on another
//! server of the network is reached through that server's peer port, where
//! the environment's grant is checked. Writes are on disk before they are
//! acknowledged, and nothing is evicted: a full store refuses writes that
//! would grow it.

pub mod export;
mod host;
mod peer;
mod store;

pub use host::{
    Binding, Home, HostConfig, HostError, KvError, KvHost, MIN_CACHE_BYTES, StoreId, TokenHash,
    generate_token, hash_token,
};
pub use peer::serve_peers;
pub use store::{Clock, KEY_OVERHEAD, Replaced, Stats, StoreError, system_clock};
