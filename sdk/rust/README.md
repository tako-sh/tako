# Tako Rust SDK

Runtime helpers for Rust apps deployed with Tako.

## Axum

Enable the `axum` feature and serve your router through Tako:

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

`tako::axum::serve` binds the listener, reports readiness when Tako provides
fd 4, and handles Tako's internal `/status` probe.

## Presence

Register spaces in a `PresenceRegistry` and pass it to the Axum adapter:

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
        // Your helper verifies the credential and access to input.params["chatId"].
        authorize_chat_presence(&input).await.map(PresenceGrant::new)
    }));

tako::axum::serve_with_presence(app, presence).await?;
```

`authorize_chat_presence` is an app-owned auth helper returning `Option<String>`
with the verified user ID. Return `None` to deny. `Some(PresenceGrant::default())`
allows anonymous access. Omit `.auth(...)` for public presence. Cookie-only auth
uses `PresenceAuth { header_name: None, cookie_name: Some("session".into()) }`.

For a custom Axum server loop, use `tako::axum::router_with_presence(app, registry)`.
It preserves internal status handling and adds authenticated `/channels/registry`
and `/channels/authorize` endpoints. Other frameworks can use `registry.metadata()`
and `registry.authorize(input).await` behind the same internal host and bootstrap
token checks.

The browser uses `usePresence("chat", { params: { chatId }, authorization: token,
data: { typing: false } })` from `tako.sh/react`. The proxy controls connection IDs,
trusted subjects, room isolation, heartbeats, and expiry. `PresenceMember<T>` and
`PresenceSnapshot<T>` expose the shared wire types. Native Rust does not add a
separate WebSocket client.

For multiple production servers, configure the shared `postgres_url` credential.
Native registrations are not detected by the JavaScript-directory deploy preflight.

## Stores

Stores serve live reads from your existing database. Register async loaders and
authorization in a `StoreRegistry`, then use the Stores-aware Axum adapter:

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

// After a database write commits:
messages.invalidate(json!({"chatId": chat_id})).await?;

tako::axum::serve_with_stores(app, stores).await?;
```

`load_messages` is your database helper returning `Result<serde_json::Value,
YourError>` from `StoreLoadInput { store, params, subject }`.
`authorize_messages` checks credentials and access and returns `Option<String>`
with the verified user ID. Omit `.auth(...)` for public reads. Reads and
subscriptions authorize independently, and loader snapshots are never broadcast.

Use `router_with_stores` for custom Axum loops. When also using presence, use
`serve_with_realtime(app, presence, stores)` or `router_with_realtime` to merge
registries. `StoreHandle::invalidate` is async with the `tokio` feature;
`invalidate_blocking` is available to synchronous callers. Invalidate exact params
after commit; an empty object is an unparameterized scope, not a wildcard.
Publication failure does not roll back the write. Use an application outbox for
durable commit-and-notify, and explicitly invalidate changes from external writers.

React uses `useStore` from `tako.sh/react`; non-React clients use `Store` from
`tako.sh/client`. Cached offline reads require `.offline(max_age_ms)` plus a
client `cacheScope` identifying the user/tenant. The default cache version is
`"1"`; set `.offline_version(...)` after `.offline(...)` and match the client
`cacheVersion` after incompatible schema changes. The browser enforces bounded
IndexedDB storage and expiry, with a maximum age of seven days. Clear the scope
with `clearStoreCache` on logout. Offline reads cannot learn of remote revocation
while disconnected. This is cached-read support; no service worker or offline
write queue is installed.

Configure shared `postgres_url` explicitly for multiple production servers; native
registrations are not discovered by the JS directory preflight. See the
[Stores guide](https://tako.sh/docs/stores/) for frontend examples and limits.

## Custom Servers

For frameworks that own the server loop, use `tako::std_listener()` or
`tako::listener().await` with the `tokio` feature. These helpers only bind and report readiness; they do not install HTTP status handling. For a custom Axum server loop, wrap the router with `tako::axum::router(app)`. Other frameworks must authenticate the internal status request and echo the token on a successful response.

Run the SDK tests from the repository root with `cargo test -p tako --all-features`.

Secrets are available through `tako::secret("NAME")` or
`tako::bootstrap()?.secret("NAME")`. Native and container releases use the same
SDK API. The SDK checks fd 3 first for native processes, then falls back to
`TAKO_BOOTSTRAP_DATA` for containers.
