---
title: Rust and Axum
description: "Serve an Axum application with Tako's Rust SDK, configure bootstrap and authenticated status handling, and package it for container deployment."
---

The Rust SDK crate is `tako`. Its `axum` feature integrates the router with Tako's listener, readiness signal, and authenticated internal status endpoint.

## Connect the router

```toml Cargo.toml
[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tako = { version = "0.2.1", features = ["axum"] }
```

```rust src/main.rs
use axum::{routing::get, Router};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let app = Router::new().route("/", get(|| async { "Hello from Tako" }));
    tako::axum::serve(app).await
}
```

Keep existing routes and middleware on the router. The SDK binds from Tako's `HOST` and `PORT` and reads bootstrap from fd 3 or container `TAKO_BOOTSTRAP_DATA`. Read private values with `tako::secret("NAME")`; see [Secrets](/secrets/).

For custom Axum server loops, wrap the router with `tako::axum::router` and use the SDK listener. `std_listener` and `listener` alone report readiness but do not install HTTP status handling. See the [Rust SDK skill](https://tako.sh/.well-known/agent-skills/tako-sdk-rust/SKILL.md) for those APIs and [Presence](/presence/) / [Stores](/stores/) for native backend registries.

## Package and verify

Run `cargo check` and `cargo test`. Package the binary with the Rust [Dockerfile](/dockerfile/) and configure a [Docker release](/docker/). The SDK handles `0.0.0.0:3000` inside the container.

There is no `runtime = "rust"` value or built-in Rust preset. Native binary deployments use explicit build/start commands; do not select an unrelated runtime to make detection succeed. Container deployment supplies a consistent build environment for the guide's example.

After deployment, request the public route and inspect finite logs. Check protected Presence/Store routes when used. Upstream: [Axum](https://docs.rs/axum/latest/axum/).
