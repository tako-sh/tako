---
name: tako-sdk-rust
description: >-
  Tako Rust SDK: bind_listener for fd-4 readiness, read_bootstrap for fd-3
  secrets/internal token, runtime metadata, HTTP framework integration,
  internal socket RPC, workflows, channels, storage helpers, and Cargo crate
  publishing checks.
type: framework
library: tako
library_version: "0.1"
sources:
  - tako-sh/tako:sdk/rust/src/lib.rs
  - tako-sh/tako:sdk/rust/README.md
---

# Tako Rust SDK (`tako` crate)

Runtime SDK for Rust apps deployed with Tako.

> **CRITICAL**: The Rust crate is named `tako`. Rust frameworks still own their
> HTTP server loop, so app code must wire the listener and internal status probe.
> The SDK does not provide a universal `ListenAndServe` wrapper like Go.

## Install

```bash
cargo add tako
```

## HTTP Apps

Use `tako::read_bootstrap()` to read the fd-3 bootstrap envelope and
`tako::bind_listener()` to bind `HOST`/`PORT` and report fd-4 readiness:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bootstrap = tako::read_bootstrap()?;
    let runtime = tako::Runtime::from_env(bootstrap.clone())?;
    let listener = tako::bind_listener()?;

    println!("{} on {}", runtime.app_name(), listener.local_addr()?);
    Ok(())
}
```

The app must answer Tako's internal health probe inside its framework router.
Use `INTERNAL_STATUS_PATH`, `INTERNAL_TOKEN_HEADER`,
`is_internal_status_request(...)`, and `internal_status_response(...)`.

Common framework shape:

```rust
if tako::is_internal_status_request(host, path, token_header, runtime.app_name(), &bootstrap) {
    return tako::internal_status_response(runtime.app_name(), &bootstrap);
}
```

Do not hardcode ports in Tako apps. In dev, `bind_listener()` defaults to
`127.0.0.1:0`; under tako-server it uses the assigned `HOST`/`PORT`.

## Runtime Context

Build runtime state from the fd-3 bootstrap envelope:

```rust
let bootstrap = tako::read_bootstrap()?;
let runtime = tako::Runtime::from_env(bootstrap)?;

let env = runtime.env();
let database_url = runtime.secret("DATABASE_URL");
```

`Runtime` exposes `env`, `is_dev`, `is_prod`, `host`, `port`, `build`,
`data_dir`, `app_name`, `base_app_name`, redacted `secrets()`, and raw
`storages()`. Use `runtime.secret("NAME")` for individual secrets. Do not log
the raw bootstrap envelope.

## Internal Socket

Use `tako::Client::from_env()` for the shared internal unix socket. It reads
`TAKO_INTERNAL_SOCKET` and `TAKO_APP_NAME`.

`Client` supports:

- `enqueue`
- `register_schedules`
- `claim`
- `heartbeat`
- `save_step`
- `complete`
- `cancel`
- `defer_run`
- `wait_for_event`
- `signal`
- `fail`
- `publish_channel`

Top-level conveniences: `tako::enqueue(...)`, `tako::signal(...)`, and `tako::publish_channel(...)`.

## Workflows

Use explicit Rust worker registration. There is no file-system auto-discovery
layer for Rust workflows yet:

```rust
let mut worker = tako::Worker::from_env()?;
worker.register("send-email", |ctx, payload| {
    let to: String = ctx.step.run("parse", || {
        Ok(payload["to"].as_str().unwrap_or_default().to_string())
    })?;
    println!("send {to}");
    Ok(())
})?;
worker.register_schedules()?;
while worker.run_once()? {}
```

`StepApi::run` checkpoints JSON-serializable step results with `save_step`.
`WorkflowContext::bail`, `WorkflowContext::fail`, `StepApi::sleep`, and
`StepApi::wait_for` map to the same server lifecycle commands as JS/Go
workflows.

Server-side code can enqueue and signal workflows:

```rust
let client = tako::Client::from_env()?;
let run = client.enqueue(
    "send-email",
    serde_json::json!({ "to": "u@example.com" }),
    tako::EnqueueOpts::default(),
)?;
client.signal("approved", serde_json::json!({ "run": run.id }))?;
```

## Channels

Rust can publish channel messages through the internal socket:

```rust
let client = tako::Client::from_env()?;
client.publish_channel("status", serde_json::json!({ "ok": true }))?;
```

Channel definition/auth helpers are not as high-level as the JS
`defineChannel(...)` authoring API yet. Use JS when a task needs generated typed
browser channel clients; use Rust for server-side publish/integration code.

## Storage

`tako::StorageBag` parses storage bindings from `runtime.storages()`. The Rust
SDK supports local signed GET/PUT URLs, S3 SigV4 signed GET/PUT URLs, optional
public S3 URLs when `public_base_url` is configured, download response header
overrides, and upload content-type signing.

```rust
let storages = tako::StorageBag::from_value(runtime.storages())?;
if let Some(uploads) = storages.get("uploads") {
    let url = uploads.create_download_url(
        "receipts/r_123.png",
        tako::UrlOptions::default(),
    )?;
}
```

## Runtime Defaults

- `runtime = "rust"`
- Dev command: `cargo run`
- Build: Cargo release build, copied to stable `app`
- Production launch: `./app`

## When Editing The Rust SDK

For SDK changes that should publish a new crate version, bump the manifest first:

```bash
just sdk-rust patch   # 0.2.0 -> 0.2.1
just sdk-rust minor   # 0.2.0 -> 0.3.0
just sdk-rust major   # 0.2.0 -> 1.0.0
```

This updates `sdk/rust/Cargo.toml` and `Cargo.lock`. Run focused validation
before committing:

```bash
cargo test -p tako --locked
cargo publish -p tako --dry-run --locked
```

If the working tree is dirty during local package validation, add
`--allow-dirty`. Never run a real `cargo publish` locally for routine changes;
the release workflow publishes the exact manifest version from `master` with
`CARGO_REGISTRY_TOKEN`.
