# E2E Tests

## CLI Output Tests

PTY-based tests that verify rendered terminal output (colors, formatting, spinners) by spawning the `tako` binary in a real pseudo-terminal via Bun's native PTY and parsing the screen with `@xterm/headless`.

```bash
just test::cli
```

Requires the `tako` binary to be built first (`cargo build -p tako`).

## Docker E2E Fixtures

From repo root:

```bash
just e2e e2e/fixtures/javascript/bun
just e2e e2e/fixtures/javascript/nextjs
just e2e e2e/fixtures/javascript/tanstack-start
```

This runs the global e2e harness in `e2e/run.sh` against the fixture path.
The harness generates an ephemeral SSH keypair per run inside a disposable Docker volume, starts real `tako-server` binaries on Ubuntu + AlmaLinux + Alpine test hosts, and never uses `~/.ssh`.
Rust build caches are stored outside the repo at `${XDG_CACHE_HOME:-~/.cache}/tako/e2e` by default:

- `cargo-home` for Cargo registry/git cache
- `target` for build outputs

Override cache locations with:

```bash
E2E_CARGO_HOME_DIR=/path/to/cargo-home E2E_CARGO_TARGET_DIR=/path/to/target ./e2e/run.sh e2e/fixtures/javascript/tanstack-start
```

After deploy, it runs universal runtime checks:

- App health endpoint responds with valid JSON.
- App root responds with valid HTML or JSON.
- Static/public files (if present in release) are fetched over HTTP.
- Compiled static assets (if present or referenced by HTML) are fetched over HTTP.
- The `channels-workflows` fixture additionally opens a real SSE stream,
  verifies direct channel publish delivery, enqueues a workflow, and verifies
  the workflow-published event arrives on the same stream.
