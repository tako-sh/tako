//! Build system for Tako
//!
//! Handles:
//! - Running build commands
//! - Creating deployment archives
//! - Build caching

mod adapter;
mod artifact;
mod container;
mod executor;
pub mod go;
pub mod js;
mod preset;
pub(crate) mod preset_cache;
pub mod schemas;
mod workdir;

pub use adapter::*;
pub use artifact::*;
pub use container::*;
pub use executor::*;
pub use preset::*;
pub use workdir::*;
