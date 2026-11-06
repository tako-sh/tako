#![allow(dead_code, unused_imports)]

pub use tako_network::*;

include!("support/tempdir.rs");

mod support;

#[path = "support/corrosion_review_http.rs"]
mod mock_http;

#[path = "../src/corrosion.rs"]
mod reviewed_adapter;

include!("support/corrosion_review_cases.rs");
