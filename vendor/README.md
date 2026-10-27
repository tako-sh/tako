# Pingora buffer lifetime

`pingora-core/` is the published `pingora-core 0.9.0` crate, with its Apache-2.0
license and a small local patch for live connection memory. Its crates.io archive
checksum is `7fd4d1ddc6d6dbc1a97164ce13c301980c25c35422aab87b512c4c8b5123729b`.
TLS test fixtures come from upstream revision
`702f69015e53f7244d6ad2e743de571d859a70a4` because the published archive omits them.

The root Cargo patch applies this same core implementation to every Pingora
consumer. This avoids build-time source rewriting and keeps releases reproducible
without an external fork. Other Pingora crates remain registry dependencies.

Changes to the published source:

- `src/protocols/l4/idle_buffer.rs` replaces Tokio's permanently allocated read
  buffer. Empty buffers return to a bounded thread-local pool when a read would
  block, reaches EOF or fails. Unread bytes remain owned by the connection.
  Transfers retain the configured read size, normally 64 KiB, and Tokio's write
  buffering. Upstream and custom PROXY sockets constructed with zero read
  capacity remain unbuffered. There are no runtime benchmark switches or
  per-read enum dispatch layers.
- `src/protocols/http/v1/server.rs` drops the completed request body's reader
  allocation after copying its final chunk and retry data. Pipelined read-ahead
  remains separately owned. This releases memory during long streaming responses.
- Focused tests cover buffer lifetime, data preservation and pipeline completion.
- CodeQL comments document test-only TLS verification bypasses for self-signed
  loopback fixtures and a load error that prints file paths without key material.

The pool retains at most eight 64 KiB buffers per polling thread (512 KiB),
independent of connection count. It does not reclaim unread data or allocations
owned by an application that stops polling. TLS offloading remains disabled.

The standalone test lockfile pins compatible upstream test dependencies. Run:

```bash
cargo test --manifest-path vendor/pingora-core/Cargo.toml --locked --no-default-features --features openssl --lib protocols::
cargo test --manifest-path vendor/pingora-core/Cargo.toml --locked --no-default-features --features openssl --lib listeners::
cargo test --locked -p tako-server proxy::
```

Keep this crate outside workspace-wide formatting and tests; its unrelated
connector tests require direct external-network access. Run the affected suites
explicitly when changing the patch. Remove the local override when a released
Pingora version provides equivalent lifetime behavior and passes Tako's memory
and performance checks. Do not update the copied crate independently of its
provenance, fixtures, focused tests and locked dependency graph.
