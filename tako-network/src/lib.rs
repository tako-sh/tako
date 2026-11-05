//! Public network identity and server-admitted membership policy.
//!
//! Registry records must come from trusted server agents. Enrollment signatures
//! prove key possession, not admission. A public profile must arrive through an
//! authenticated channel; validating its contents cannot establish its provenance.

pub mod api;
mod challenge;
mod enrollment;
mod error;
mod keys;
mod profile;
mod registry;
mod types;

pub use challenge::*;
pub use enrollment::*;
pub use error::{Error, Result};
pub use keys::LocalDeviceKeys;
pub use profile::{NetworkPublicProfile, PendingApprovalProfile};
pub use registry::*;
pub use types::*;
