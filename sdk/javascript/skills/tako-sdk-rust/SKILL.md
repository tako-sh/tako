---
name: tako-sdk-rust
description: >-
  tako.sh Rust SDK: bootstrap secrets, listener binding, and Axum serving with
  Tako internal status handling.
type: framework
library: tako.sh
library_version: "0.1.0"
sources:
  - tako-sh/tako:sdk/rust
---

# Tako Rust SDK

Runtime SDK for Rust apps deployed with Tako.

Use the SDK for native and container releases. Do not tell users to manually
bind from `PORT` or implement `/status`; the SDK owns the runtime contract.

## Axum

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

## Custom Servers

Use `tako::std_listener()` or `tako::listener().await` with the `tokio` feature
when a framework owns its server loop.

Secrets are available through `tako::secret("NAME")` or
`tako::bootstrap()?.secret("NAME")`.
