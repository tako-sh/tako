---
name: tako-sdk-rust
description: >-
  tako.sh Rust SDK: bootstrap secrets, listener binding, and Axum serving with
  Tako internal status handling, authenticated presence, and live Stores with cached offline reads.
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

The app-owned `authorize_chat_presence` helper returns `Option<String>` with a
verified user ID after checking credentials and access to `input.params["chatId"]`.
The example maps that ID to `PresenceGrant`. The `.auth(...)` callback itself
returns `Option<PresenceGrant>`: `None` denies, while
`Some(PresenceGrant::default())` allows an anonymous connection. Omit
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

Local Presence connections bypass auth callbacks and use in-memory state.
Verify private-room rejection and trusted identities in a deployed test
environment. Deduplicate subjects when counting users across tabs; connection
IDs identify sessions, not people.

## Stores

Use `StoreRegistry` and `StoreDefinition` to expose live reads from an existing
database. A `StoreHandle` invalidates exact params after a committed write:

```rust
use std::sync::Arc;
use serde_json::json;
use tako::{StoreAuth, StoreDefinition, StoreError, StoreGrant, StoreRegistry};

let stores = Arc::new(StoreRegistry::new());
let messages = stores.register("messages", StoreDefinition::new(|input| async move {
    load_messages(input).await.map_err(StoreError::loader)
})
    .params_schema(json!({
        "type": "object",
        "properties": {"chatId": {"type": "string"}},
        "required": ["chatId"]
    }))
    .auth(StoreAuth::default(), |input| async move {
        authorize_messages(&input).await.map(StoreGrant::new)
    })
    .offline(3_600_000))?;

// Commit the database write first:
messages.invalidate(json!({"chatId": chat_id})).await?;
tako::axum::serve_with_stores(app, stores).await?;
```

`load_messages` is an app-owned async helper returning `Result<serde_json::Value,
YourError>` from `StoreLoadInput { store, params, subject }`.
`authorize_messages` verifies credentials and scope access and returns
`Option<String>` with the verified subject. Omit `.auth(...)` for public reads.
Loader snapshots are private to each reader; only invalidation signals fan out.
Reads and subscriptions each enforce schema validation and authorization in dev
and production. Store names follow `^[A-Za-z0-9][A-Za-z0-9._-]{0,255}$`.

For a custom Axum loop, use `router_with_stores(app, stores)`. To combine presence
and Stores, use `serve_with_realtime(app, presence, stores)` or
`router_with_realtime`; these merge both registries. Other frameworks must protect
`metadata()`, `authorize(...).await`, and `load(...).await` with the internal host
and bootstrap token. `StoreHandle::invalidate` is async with the `tokio` feature;
use `invalidate_blocking` only in synchronous code or on a blocking pool.

React frontends use `useStore<Data>("messages", options)` from `tako.sh/react`.
`.offline(max_age_ms)` permits opt-in persisted reads; the client must also supply
a stable, non-secret `cacheScope` for the user/tenant. Cache version defaults to
`"1"`; call `.offline_version(...)` after `.offline(...)`, and match frontend
`cacheVersion` after incompatible schema changes. The default browser cache is
bounded IndexedDB with TTL capped at seven days. It stores no auth credentials.
Clear the scope with `clearStoreCache` on logout and disable/unmount readers.
Offline reads cannot detect remote revocation, install a PWA, or queue writes.

Invalidate only after commit, using the exact params; `{}` is not a wildcard.
External writers must invalidate explicitly; Tako does not inspect database
dependencies. Publication errors do not roll back or retry the database write;
use an app outbox for durable commit-and-notify. Params are capped at 16 KiB and
read responses at 1 MiB. Multi-server native apps must explicitly configure shared
`postgres_url`; JS directory detection does not find Rust registrations. See
https://tako.sh/docs/stores/ for the frontend lifecycle and verify scope isolation,
reconnection, and offline expiry.

Run the application's tests and exercise its routes through the Tako-provided
URL. For authenticated Stores, verify both read and subscription rejection;
unlike Presence, Store auth is enforced during local development too.
