# tako-server

Rust crate for the remote Tako runtime and proxy.

## Responsibilities

- Start/stop/manage app instances.
- Maintain route table and app load balancers.
- Terminate HTTP/HTTPS traffic and proxy upstream.
- Redirect HTTP traffic to HTTPS (except ACME challenge checks).
- Cache proxied `GET`/`HEAD` upstream responses in-memory when response cache directives explicitly allow caching.
- Perform health probing using `Host: <app>.tako` + `/status` against each app instance.
- Perform active health probing.
- Serve management commands over Unix socket.
- Report per-build runtime status (multiple concurrently running builds during rollout).
- Validate on-demand deploy startup when the desired instance count is `0` before finalizing idle state.
- Validate app ids, release ids, and deploy paths at the management socket boundary.
- Persist app runtime registration (config/routes + release metadata) to SQLite and restore it on restart.
- Read non-secret env vars from release `app.json` and secrets from encrypted SQLite state, then pass the shared bootstrap envelope to native processes on fd 3 or containers through `TAKO_BOOTSTRAP_DATA`.
- Create, encrypt, index, prune, download, and restore private app data backups when an app environment enables backups.
- Serve durable channel pub-sub over `GET /_tako/channels/<name>` using SSE or WebSocket negotiation, with bounded app/env replay history in local SQLite or shared Postgres.
- Serve public optimized WebP image URLs under `/_tako/image`, with optional AVIF when configured, request guardrails, short-lived source caching, queued isolated child-process transforms, same-key in-flight dedupe, and a pruned origin disk cache for successful variants.
- Persist server upgrade mode in SQLite and reject mutating commands while upgrading.
- Use a single-owner durable upgrade lock so only one upgrade controller can enter upgrading mode at a time.
- Expose `server_info`, `enter_upgrading`, and `exit_upgrading` management commands for upgrade orchestration.
- Serve signed remote management RPC over Tailscale-bound HTTP when `--management-host` is configured.
- Enable zero-downtime reload handoff with SIGHUP child spawn, `SO_REUSEPORT` listener overlap, and pid-specific management sockets (`tako-{pid}.sock`) behind stable symlink `tako.sock`.

Routing policy notes:

- Deploy commands must include at least one non-empty route.
- No implicit catch-all/no-routes mode is supported.

## Key Runtime Paths

- Socket: `/var/run/tako/tako.sock`
- Public HTTP/HTTPS: `--http-port` and `--https-port` (defaults: `80` and `443`)
- Remote management HTTP: `9844` on the configured Tailscale address
- Data root: `/opt/tako`
- State DB: `/opt/tako/state.sqlite`
- Server identity: `/opt/tako/identity.key`, `/opt/tako/identity.pub`
- Remote management keys: `/opt/tako/management-authorized-keys`
- App releases: `/opt/tako/apps/<app>/<env>/releases/<version>/`
- App data: `/opt/tako/apps/<app>/<env>/data/app/` and `/opt/tako/apps/<app>/<env>/data/tako/`
- Image source cache: in-memory, scoped to each app release.
- Image transform cache: system temp directory, usually `/tmp/tako-image-cache`.

## Run and Test

Deployment hosts require Linux with glibc and systemd on x86_64 or ARM64.

Install libvips first: macOS `brew install vips`; Debian/Ubuntu `sudo apt-get update && sudo apt-get install -y --no-install-recommends libvips-dev`.

Homebrew's `vips` formula includes the codec libraries Tako needs for JPEG, PNG, WebP, and AVIF transforms. Debian/Ubuntu split AVIF encoder and decoder support into optional `libheif` plugin packages such as `libheif-plugin-aomenc`, `libheif-plugin-aomdec`, and `libheif-plugin-dav1d` when they are available.

From the repository root:

Build a Linux release for one architecture with `just build::tako-server x86_64` or `just build::tako-server aarch64`. Use `just build::tako-server-all` for both. These builds target glibc and require cargo-zigbuild and Zig.

```bash
cargo test -p tako-images
cargo run -p tako-server -- --help
cargo test -p tako-server
```

Behavior tests use `isolation::fixture::TestDataDir` to opt a temporary data root into
local-account provisioning. They exercise the real filesystem permission walkers;
unregistered roots still require the installed Linux isolation configuration.
Account switching and cgroup limits are covered by the installed isolation tests.
When running the test executable directly in Docker, use `--init` so its children
do not mistake a PID 1 test runner for an exited parent.

Example local run:

```bash
cargo run -p tako-server -- \
  --socket /tmp/tako.sock \
  --http-port 8080 \
  --https-port 8443 \
  --data-dir /tmp/tako-data \
  --management-host 100.64.0.10 \
  --no-acme
```

## Related Docs

- `website/src/pages/docs/quickstart.astro` (remote server install + first deploy setup)
- `website/src/pages/docs/deployment.md` (deploy flow and runtime expectations)
- `website/src/pages/docs/how-tako-works.md` (runtime component/data-flow context)
- [`../PROTOCOL.md`](../PROTOCOL.md) (cross-component runtime contracts)
