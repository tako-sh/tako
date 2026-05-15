//! TLS/Certificate management
//!
//! Handles:
//! - ACME (Let's Encrypt) certificate issuance via HTTP-01 challenge
//! - Certificate lifecycle management with automatic renewal
//! - Self-signed certificates for development
//! - SNI-based certificate selection

mod acme;
mod dns;
mod manager;
mod self_signed;
mod sni;

#[allow(unused_imports)]
pub use acme::{AcmeClient, AcmeConfig, AcmeError, ChallengeHandler, ChallengeTokens};
#[allow(unused_imports)]
pub use manager::{CertError, CertInfo, CertManager, CertManagerConfig};
#[allow(unused_imports)]
pub use self_signed::{SelfSignedCert, SelfSignedError, SelfSignedGenerator};
#[allow(unused_imports)]
pub use sni::{SniCertResolver, create_sni_callbacks};
