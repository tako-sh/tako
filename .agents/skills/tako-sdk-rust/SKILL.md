---
name: tako-sdk-rust
description: >-
  tako.sh Rust SDK: bootstrap secrets, listener binding, and Axum serving with
  Tako internal status handling and authenticated presence registries.
type: framework
library: tako.sh
library_version: "0.2.1"
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
tako = { version = "0.2.1", features = ["axum"] }
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
when a framework owns its server loop. These helpers report readiness but do not wrap HTTP requests. For a custom Axum loop, use `tako::axum::router(app)` to retain internal status authentication. Other frameworks need their own authenticated status handler.

Secrets are available through `tako::secret("NAME")` or
`tako::bootstrap()?.secret("NAME")`.

## Presence

Use `PresenceRegistry` and `PresenceDefinition` for a Rust backend consumed by
`usePresence` from `tako.sh/react`. No native WebSocket client is provided.

```rust
use std::sync::Arc;
use tako::{PresenceAuth, PresenceDefinition, PresenceGrant, PresenceRegistry};
use serde_json::json;

let presence = Arc::new(PresenceRegistry::new());
presence.register("chat", PresenceDefinition::new()
    .params_schema(json!({
        "type": "object",
        "properties": {"chatId": {"type": "string"}},
        "required": ["chatId"]
    }))
    .max_connection_lifetime_ms(60_000)
    .auth(PresenceAuth::default(), |input| async move {
        authorize_chat_presence(&input).await.map(PresenceGrant::new)
    }));
tako::axum::serve_with_presence(app, presence).await?;
```

The app-owned `authorize_chat_presence` helper returns a verified user ID after
checking the credential and access to `input.params["chatId"]`. Return `None` to
deny, or `Some(PresenceGrant::default())` to allow an anonymous connection. Omit
`.auth(...)` for a public space. Cookie-only auth uses `PresenceAuth { header_name:
None, cookie_name: Some("session".into()) }`. Async verifiers receive the public
space name, operation, schema-validated params, header, and cookie.

`router_with_presence(app, registry)` wraps a custom Axum loop. Both presence
adapter helpers preserve status probes and protect internal registry/auth routes
with the bootstrap token. Other frameworks must protect `metadata()` and
`authorize(input).await` with the same internal host and token checks.

The browser connects by public name with `params: { chatId }`. Validated params
partition membership; verified subjects identify users, while each tab has its
own connection ID. Metadata is untrusted client JSON. The proxy and React hook
manage leases, disconnect cleanup, and reconnects. Every authorized member of a
scope sees its roster; permissions are checked again when the configured maximum
connection lifetime expires. Configure the shared `postgres_url` credential for
multiple servers; native registration is not detected by the JS deploy preflight.

Run `cargo test -p tako` and `cargo test -p tako --all-features` after changes.
