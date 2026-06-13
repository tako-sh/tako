# Tako Rust SDK

Runtime helpers for Rust apps deployed with Tako.

## Axum

Enable the `axum` feature and serve your router through Tako:

```toml
[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tako = { version = "0.1", features = ["axum"] }
```

```rust
use axum::{routing::get, Router};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let app = Router::new().route("/", get(|| async { "Hello from Tako" }));
    tako::axum::serve(app).await
}
```

`tako::axum::serve` binds the listener, reports readiness when Tako provides
fd 4, and handles Tako's internal `/status` probe.

## Custom Servers

For frameworks that own the server loop, use `tako::std_listener()` or
`tako::listener().await` with the `tokio` feature.

Secrets are available through `tako::secret("NAME")` or
`tako::bootstrap()?.secret("NAME")`. Native and container releases use the same
SDK API. The SDK checks fd 3 first for native processes, then falls back to
`TAKO_BOOTSTRAP_DATA` for containers.
