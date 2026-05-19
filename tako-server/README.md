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
- Read non-secret env vars from release `app.json` and secrets from encrypted SQLite state, then push secrets to instances over the internal HTTP endpoint.
- Serve durable channel pub-sub over `GET /_tako/channels/<name>` using SSE or WebSocket negotiation, with a bounded per-app replay window stored locally.
- Serve public optimized AVIF/WebP image URLs under `/_tako/image`, with request guardrails, queued isolated child-process transforms, and a best-effort origin disk cache for successful variants.
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
- State DB: `/opt/tako/tako.db`
- Server identity: `/opt/tako/identity.key`, `/opt/tako/identity.pub`
- Remote management keys: `/opt/tako/management-authorized-keys`
- App releases: `/opt/tako/apps/<app>/<env>/releases/<version>/`
- Image transform cache: system temp directory, usually `/tmp/tako-image-cache`.

## Run and Test

From repository root:

Install libvips first: macOS `brew install vips`; Debian/Ubuntu `sudo apt-get update && sudo apt-get install -y --no-install-recommends libvips-dev libheif-plugin-aomenc`.

Homebrew's `vips` formula includes the codec libraries Tako needs for JPEG, PNG, WebP, and AVIF transforms. Debian/Ubuntu split the AVIF encoder into `libheif-plugin-aomenc`, so install that alongside `libvips-dev`.

```bash
cargo test -p tako-images
cargo run -p tako-server -- --help
cargo test -p tako-server
```

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
