---
name: tako-sdk-rust
description: >-
  Tako Rust SDK: bind_listener for fd-4 readiness, read_bootstrap for fd-3
  secrets/internal token, runtime metadata, internal socket RPC, workflows,
  channels, storage helpers, and internal status helpers for Rust HTTP frameworks.
type: framework
library: tako
sources:
  - tako-sh/tako:sdk/rust/src/lib.rs
---

# Tako Rust SDK (`tako` crate)

Runtime SDK for Rust apps deployed with Tako.

## Install

```bash
cargo add tako
```

## HTTP Apps

Use `tako::read_bootstrap()` to read the fd-3 envelope and `tako::bind_listener()` to bind `HOST`/`PORT` and report fd-4 readiness.

Rust frameworks still own their HTTP server loop. The SDK exposes `INTERNAL_STATUS_PATH`, `INTERNAL_TOKEN_HEADER`, `is_internal_status_request(...)`, and `internal_status_response(...)` so apps can answer Tako's internal health probe.

## Runtime Context

Build runtime state from the fd-3 bootstrap envelope:

```rust
let bootstrap = tako::read_bootstrap()?;
let runtime = tako::Runtime::from_env(bootstrap)?;

let env = runtime.env();
let database_url = runtime.secret("DATABASE_URL");
```

`Runtime` exposes `env`, `is_dev`, `is_prod`, `host`, `port`, `build`, `data_dir`, `app_name`, `base_app_name`, redacted `secrets()`, and raw `storages()`.

## Internal Socket

Use `tako::Client::from_env()` for the shared internal unix socket. It reads `TAKO_INTERNAL_SOCKET` + `TAKO_APP_NAME` and falls back to `TAKO_WORKFLOW_SOCKET` for older Go-style env naming.

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

Use explicit Rust worker registration:

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

`StepApi::run` checkpoints JSON-serializable step results with `save_step`. `WorkflowContext::bail`, `WorkflowContext::fail`, `StepApi::sleep`, and `StepApi::wait_for` map to the same server lifecycle commands as JS/Go workflows.

## Storage

`tako::StorageBag` parses storage bindings from `runtime.storages()`. The Rust SDK supports local signed GET/PUT URLs, S3 SigV4 signed GET/PUT URLs, optional public S3 URLs when `public_base_url` is configured, download response header overrides, and upload content-type signing.

## Runtime Defaults

- `runtime = "rust"`
- Dev command: `cargo run`
- Build: Cargo release build, copied to stable `app`
- Production launch: `./app`
