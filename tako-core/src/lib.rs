//! Tako Core - Shared protocol types
//!
//! This crate contains the protocol types shared between the Tako CLI (`tako`)
//! and the Tako server (`tako-server`) for communication via Unix sockets.
//!
//! All CLI-specific functionality (SSH, build, runtime detection, local dev CA, etc.)
//! lives in the `tako` crate.

pub mod bootstrap;
pub mod compose;
pub mod container;
mod error_code;
mod error_codes;
pub mod instance_env;
pub mod network;
mod protocol;
pub mod storage;
#[cfg(test)]
mod wire_fixtures;

pub use error_code::{ErrorCode, SocketError};
pub use protocol::*;
pub use storage::*;
