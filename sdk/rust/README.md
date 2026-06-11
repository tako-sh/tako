# Tako Rust SDK

Rust SDK crate for Tako applications.

## Install

```bash
cargo add tako
```

## HTTP Apps

Use `tako::bind_listener()` to bind the Tako-provided `HOST`/`PORT` and report fd-4 readiness:

```rust
let listener = tako::bind_listener()?;
```

The helper binds to `127.0.0.1:0` under Tako by default and writes the selected port to fd 4.

Use `tako::read_bootstrap()` to read the fd-3 bootstrap envelope, then build a runtime view:

```rust
let bootstrap = tako::read_bootstrap()?;
let runtime = tako::Runtime::from_env(bootstrap)?;

let app = runtime.base_app_name();
let database_url = runtime.secret("DATABASE_URL");
```

Rust frameworks still own their HTTP loop. Use `tako::is_internal_status_request(...)` and `tako::internal_status_response(...)` to answer `Host: <app>.tako` + `GET /status` health probes.

## Workflows And Channels

Use `tako::Client` for the shared internal unix socket (`TAKO_INTERNAL_SOCKET` + `TAKO_APP_NAME`):

```rust
let client = tako::Client::from_env()?;
let run = client.enqueue(
    "send-email",
    serde_json::json!({ "to": "u@example.com" }),
    tako::EnqueueOpts::default(),
)?;

client.publish_channel("status", serde_json::json!({ "run": run.id }))?;
```

Workers use `tako::Worker`:

```rust
let mut worker = tako::Worker::from_env()?.with_worker_id("email-1");
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

The SDK only talks to tako-server over the internal socket. It does not open or manage workflow storage.

## Storage

Storage bindings are delivered in the fd-3 bootstrap envelope. `tako::StorageBag` parses the binding map and supports local signed GET/PUT URLs plus S3 public URLs when `public_base_url` is configured.

## Test

```bash
cargo test -p tako
```
