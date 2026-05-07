---
layout: ../../layouts/DocsLayout.astro
title: "How Tako works: rolling deploys, TLS, health checks, and scale to zero - Tako Docs"
heading: "How Tako Works"
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako is a local development and self-hosted deployment platform. The local `tako` CLI builds your app, uses Tailscale for private remote management, and keeps SSH for server setup and recovery. The server runs the proxy, manages app processes, stores runtime state, handles TLS, and performs rolling updates.

The protocol is v0, so Tako keeps the system small and direct: runtime behavior lives in plugins, presets only provide framework metadata, and deployed apps are identified as `{app}/{env}` on each server.

## The Main Pieces

### `tako` CLI

The CLI is the control plane you run from your machine. It handles:

- Project setup with `tako init`
- Local HTTPS development with `tako dev`
- Builds and artifact uploads with `tako deploy`
- Server inventory in global `config.toml`
- Encrypted project secrets in `.tako/secrets.json`
- Logs, releases, rollbacks, scaling, and delete operations

App-scoped commands read `./tako.toml` by default. Use `-c path/to/config` to select another config file; the config file's parent directory becomes the app directory.

### `tako-server`

`tako-server` runs on your hosts. It owns:

- HTTP and HTTPS listeners
- TLS certificates and ACME renewal
- Route matching and load balancing
- App process supervision
- Rolling deploys and rollbacks
- Scale-to-zero cold starts
- Static asset serving from deployed `public/`
- Channel storage, workflow queues, and internal sockets
- Prometheus metrics on localhost

By default it uses `/opt/tako` for data and `/var/run/tako/tako.sock` for local management commands. Normal server installs also bind private remote management to the server's Tailscale address on port `9844`.

### `tako.sh` SDK

Apps use the SDK to satisfy Tako's runtime contract. JavaScript and TypeScript apps export a Web Standard fetch handler. Go apps use the `tako` package around an `http.Handler`.

The SDK handles:

- Binding to the private loopback host/port Tako provides
- Signaling readiness on fd 4
- Reading the fd 3 bootstrap envelope for internal auth and secrets
- Serving the internal status endpoint
- Channel auth, dispatch, and registry endpoints
- Workflow enqueue and worker RPC helpers

## Deployment Flow

A production deploy starts locally and finishes on each server:

1. The CLI validates `tako.toml`, routes, secrets, server metadata, and the target environment.
2. The CLI resolves the source root, app directory, runtime, package manager, preset, and entrypoint.
3. The app is copied into `.tako/build`, respecting `.gitignore` and symlinking local `node_modules/` for build tools.
4. Build stages run in order. If no custom build exists, Tako uses the runtime default build when one exists.
5. Assets from presets and top-level `assets` are merged into the app `public/` directory.
6. The CLI verifies the resolved runtime `main` file exists and packages a filtered artifact.
7. The artifact is uploaded to every target server, extracted into a new release directory, and prepared with the runtime plugin's production install command.
8. If a release command is configured, the leader server runs it before any rolling update begins.
9. `tako-server` starts new instances, waits for health, adds them to the load balancer, drains old instances, and updates the `current` symlink.

If a deploy fails after creating a release directory, Tako cleans up the partial release. If a new instance fails health checks during rolling update, the previous release keeps serving.

## Routing

Routes are declared per environment in `tako.toml`:

```toml
[envs.production]
routes = [
  "example.com",
  "www.example.com",
  "example.com/api/*",
  "*.tenant.example.com",
]
servers = ["la", "nyc"]
```

The proxy matches by host and path, then chooses the most specific route. Exact hosts beat wildcards, and longer paths beat shorter paths. After a request matches an app route, `/channels/<name>` is reserved for Tako channels. Static asset paths with file extensions are served directly from `public/` when present; everything else is proxied to an app instance.

Unmatched production routes return `404`. In local dev, unknown managed `.test` and `.tako.test` hosts return a helpful `421` that lists registered dev routes.

## Process Model

Deployed instances bind to loopback only. Tako sets:

- `HOST=127.0.0.1`
- `PORT=0`
- `ENV=<environment>`
- `TAKO_BUILD=<version>`
- `TAKO_DATA_DIR=<persistent app data dir>`
- `TAKO_APP_NAME=<deployment id>`
- `TAKO_INTERNAL_SOCKET=<shared internal socket>`

The app binds an OS-assigned port and writes that port to fd 4. `tako-server` then routes traffic and health probes to the private TCP endpoint.

Secrets and the per-instance internal token arrive through fd 3 as JSON before user code runs. Secrets are not written to a release `.env` file and are not inherited by subprocesses through environment variables.

## Health Checks

Tako uses active health probes as the source of truth:

```http
GET /status
Host: tako.internal
X-Tako-Internal-Token: <instance-token>
```

The SDK implements the response automatically. During startup, probes run faster so cold-start readiness is detected quickly. After an instance is healthy, one failed probe marks it dead and triggers replacement.

Production 5xx responses stay generic. Detailed startup, proxy, channel, and static-file diagnostics go to logs instead of browser response bodies.

## Scaling

Desired instance count is stored on each server, not in `tako.toml`.

- New deployments default to one desired instance.
- `tako scale N` changes desired instances per targeted server.
- `N > 0` keeps at least that many instances running.
- `N = 0` enables scale-to-zero.

Scale-to-zero apps keep one warm instance immediately after deploy. After the idle timeout, instances stop. The next request triggers a cold start and waits for readiness. If startup times out, the proxy returns `504`; if startup fails, it returns `502`; if too many requests are waiting for a cold start, it returns `503`.

## TLS

Production TLS uses SNI and automatic certificate management:

- Public hostnames use Let's Encrypt through HTTP-01.
- Wildcard routes use DNS-01 through lego after `tako servers setup-wildcard`.
- Private or local hostnames use self-signed certificates.
- Certificates renew 30 days before expiry.
- If no exact cert exists, Tako tries a wildcard fallback, then serves a default certificate so HTTP routing can return a normal status.

Local development uses a Tako development CA. The cert is stored at `{TAKO_HOME}/ca/ca.crt`, the private key at `{TAKO_HOME}/ca/ca.key`, and system trust is installed once through sudo.

## Workflows And Channels

Channels are durable pub-sub streams at `/channels/<name>` on your app routes. SSE clients receive replay plus live messages. WebSocket clients can also send frames that route through declared channel handlers.

Workflows are durable background runs stored in a per-app SQLite queue. `tako-server` owns the database, cron ticker, leases, and worker supervision. SDKs only talk to the internal socket. Workers are separate from HTTP instances so background dependencies and long-running work do not affect request serving.

## Persistence

Each deployed app has:

```text
/opt/tako/apps/{app}/{env}/
  current -> releases/{version}
  data/
    app/      # exposed as TAKO_DATA_DIR
    tako/     # Tako internal state
  logs/
    current.log
  releases/{version}/
    app.json
    ...
```

App registration, routes, desired scale, secrets, upgrade locks, and workflow state are persisted so reloads and restarts can recover cleanly.
