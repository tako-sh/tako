#![allow(dead_code, unused_imports)]

pub use tako_network::*;

// The adapter is compiled in this crate to inject its test transport. Compile
// its shared-state dependency here too, keeping migration internals private.
#[path = "../src/shared.rs"]
pub mod shared;

include!("support/tempdir.rs");

mod support;

#[path = "support/corrosion_review_http.rs"]
mod mock_http;

#[path = "../src/corrosion.rs"]
mod reviewed_adapter;

include!("support/corrosion_review_cases.rs");
