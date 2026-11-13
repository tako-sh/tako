//! Public network identity and server-admitted membership policy.
//!
//! Registry records must come from trusted server agents. Enrollment signatures
//! prove key possession, not admission. A public profile must arrive through an
//! authenticated channel; validating its contents cannot establish its provenance.

pub mod api;
mod bootstrap;
mod challenge;
pub mod corrosion;
mod enrollment;
mod error;
pub mod invitation;
mod keys;
pub mod node_config;
mod profile;
mod registry;
pub mod runtime;
pub mod sealed;
mod server_identity;
pub mod services;
pub mod shared;
mod types;

pub use bootstrap::{BootstrapRequest, BootstrapResponse, MAX_BOOTSTRAP_BYTES};
pub use challenge::*;
pub use enrollment::*;
pub use error::{Error, Result};
pub use keys::LocalDeviceKeys;
pub use node_config::{NodeConfig, NodeFiles};
pub use profile::{NetworkPublicProfile, PendingApprovalProfile};
pub use registry::*;
pub use server_identity::{ServerIdentity, ServerTlsPaths};
pub use types::*;

#[cfg(test)]
include!("../tests/support/tempdir.rs");
