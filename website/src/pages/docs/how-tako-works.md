---
layout: ../../layouts/DocsLayout.astro
title: "How Tako works: rolling deploys, TLS, health checks, and scale to zero - Tako Docs"
heading: How Tako Works
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako is a deployment and development platform that takes JavaScript, TypeScript, and Go apps from local development to production with minimal configuration. It builds your app, ships it to your servers, routes traffic, manages TLS, watches instance health, scales up and down, and rolls updates out without downtime.

This page is a tour of the moving parts. It covers the three components that make up Tako, the two paths requests can take, and the pieces that actually do the work — routing, health checks, scaling, TLS, caching, workflows, and channels.

## The Three Components

Tako is three programs working together.

**`tako` CLI** runs on your laptop. You use it for project setup (`tako init`), local development (`tako dev`), deploys (`tako deploy`), scaling (`tako scale`), and for managing servers and secrets. It talks to your servers over SSH.

**`tako-server`** runs on each deployment host. It spawns and supervises your app's instances, terminates TLS, routes incoming traffic through a Pingora-based proxy, runs health probes, issues ACME certificates, and performs rolling updates. It is the only thing on the box that needs to know about Tako.

**`tako.sh` SDK** is the library you import from your app. The JavaScript/TypeScript package (`npm install tako.sh`) ships runtime adapters for Bun, Node, and Deno plus a fetch-handler interface. The Go module (`go get tako.sh`) gives you `tako.ListenAndServe(handler)` for any `http.Handler`. Both implementations handle the Tako protocol: readiness signaling, the built-in health endpoint, secret delivery, and graceful shutdown.

## Two Paths: Management and Traffic

Everything Tako does falls into one of two paths.

The **management path** is where state changes happen. `tako deploy`, `tako scale`, `tako secrets sync`, `tako servers upgrade` — these travel over SSH to a Unix socket on the target server and speak a small JSON protocol to `tako-server`.

The **traffic path** is the HTTP/HTTPS request path. Real user traffic lands on port 443 (or 80), flows through the Pingora proxy inside `tako-server`, and is forwarded to an app instance over a private TCP endpoint on loopback. The CLI never touches this path.

Keeping these paths separate means commands and live traffic never compete for the same resources. The proxy stays hot; commands stay controllable.

## Local Development

`tako dev` spins up a local HTTPS environment on `.test` domains with a real browser-trusted certificate:

```
$ tako dev
# App is live at https://my-app.test
```

Under the hood, the CLI is a thin client. A persistent daemon called `tako-dev-server` does the actual work — it owns the app process, holds the HTTPS listener, and keeps routing tables across multiple apps at once.

When you run `tako dev`:

1. The CLI makes sure the daemon is running (starting it if it isn't).
2. It registers the selected config file with the daemon, which persists the registration in a local SQLite database.
3. One instance of your app starts immediately.
4. HTTPS is terminated by the daemon using a local Certificate Authority — once trusted, the browser shows a clean padlock.

Your app stays running while you work. Press `b` to background the session: the CLI exits but the daemon keeps the app alive. Running `tako dev` again reattaches. Press `Ctrl+c` to stop the app entirely. After 30 minutes with no attached CLI client, the daemon idles the process and restarts it on the next HTTP request.

On macOS the daemon installs a socket-activated `tako-dev-proxy` (a one-time sudo prompt) so your app is reachable on the standard ports without running the CLI as root. On Linux, a similar effect is achieved with iptables redirect rules and a dedicated loopback alias (`127.77.0.1`).

### Variants

Pass `--variant` (or `--var`) to run a named variant of the same app on its own subdomain:

```
$ tako dev --variant admin
# Live at https://my-app-admin.test
```

Useful for running two configurations side by side without stepping on each other's routes.

### Dev Command Resolution

`tako dev` decides how to run your app in this order:

1. A top-level `dev` array in `tako.toml` (for example `dev = ["vite", "dev"]`).
2. The `dev` field on your resolved preset (the `tanstack-start` preset runs `vite dev`, the `nextjs` preset runs `next dev`).
3. A runtime default: JavaScript runtimes go through the SDK's dev entrypoint, which wraps your default-exported fetch handler into a real HTTP server. Go runs `go run .`.

The daemon activates a dev route only after the app writes its bound loopback port to fd 4. Direct Vite dev commands need the `tako.sh/vite` plugin for that readiness signal; Tako treats Vite's printed localhost URL as a log line, not as readiness.

The SDK's dev entrypoint is the same code path as production, so what you see locally is the shape of what you'll see in prod.

### Config Watching

`tako dev` watches `tako.toml` while it runs. If your effective dev environment variables change, the app restarts. If `[envs.development].route(s)` changes, routing updates live.

Source hot-reload is left to the runtime (Bun's `--watch`, Vite's HMR, `next dev`, and so on). Tako deliberately does not watch source files — your framework does that better.

## Deploying to Production

Deployment is one command:

```
$ tako deploy
```

That single command does validation, a local build, an upload, and a rolling update.

### 1. Validate and Prepare

Tako validates the selected config, resolves your app name, checks that required secrets are present, and confirms each target server has the architecture and libc metadata it captured when you first added it.

### 2. Build Locally

The build always runs on your machine, never on the server. Tako:

- Copies your project into a clean `.tako/build` directory, honoring `.gitignore`.
- For JavaScript runtimes, it symlinks `node_modules/` from your original tree so installs don't run twice, and restores known build caches (`.turbo/`, `.next/cache/`) into the build dir.
- Runs your build commands: `[[build_stages]]` if you defined them, otherwise `[build]`, otherwise a runtime default (for JS that's `<pm> run --if-present build`).
- Merges configured asset directories into `public/`.
- Verifies the resolved entrypoint actually exists in the built workspace.
- Packages the result into a deploy artifact. `node_modules/` and local build caches are excluded — for JS the server installs production deps; for Go the binary is self-contained.
- Caches the artifact locally, so a second deploy with unchanged inputs is nearly instant.

### 3. Upload and Deploy

For each target server, in parallel:

- A disk space preflight runs under `/opt/tako` before upload.
- The artifact is uploaded and extracted.
- Tako queries the server's current secrets hash; if it matches, secrets stay as-is. If it doesn't, the new secrets travel with the deploy command.
- A `prepare_release` call downloads the right runtime version and installs production dependencies.
- A `deploy` call tells `tako-server` to perform a first start or a rolling update.
- The `current` symlink flips to the new release, and releases older than 30 days are pruned.

If a deploy fails after creating a release directory, Tako cleans up the partial release on its way out.

### Version Naming

Deploy versions are derived from your git state:

| Git state  | Version format    | Example            |
| ---------- | ----------------- | ------------------ |
| Clean tree | `{commit}`        | `abc1234`          |
| Dirty tree | `{commit}_{hash}` | `abc1234_def56789` |
| No git     | `nogit_{hash}`    | `nogit_def56789`   |

## Traffic Routing

When a request hits your server, `tako-server` handles it like this:

1. The request arrives on port 80 or 443.
2. HTTP requests are redirected to HTTPS with a `307` (ACME challenges on `/.well-known/acme-challenge/*` stay on HTTP).
3. The router matches the `Host` header and path against every deployed app's routes.
4. The most specific match wins — exact hostnames beat wildcards, and longer path prefixes beat shorter ones.
5. For paths that look like static assets (anything with a file extension), Tako tries the app's `public/` directory first. For path-prefixed routes like `example.com/app/*`, the prefix is stripped when looking up the file.
6. Otherwise the request is proxied to a healthy instance, picked by round-robin load balancing.
7. If nothing matches, the response is a `404`.

### Route Patterns

Routes live per-environment in `tako.toml` and support several patterns:

```toml
[envs.production]
routes = [
  "api.example.com",           # Exact hostname
  "*.example.com",             # Wildcard subdomain
  "example.com/api/*",         # Hostname + path prefix
  "*.example.com/admin/*",     # Wildcard + path prefix
]
```

Different apps can share a server by owning different routes. Conflicting routes are caught at deploy time rather than in production.

### Limits

Tako limits each client IP to 2048 concurrent connections; requests beyond that receive `429`. Request bodies larger than 128 MiB receive `413`.

## Health Checks and Instance Lifecycle

Every app instance is actively probed:

- **Probe interval**: once per second.
- **Probe request**: `GET /status` with `Host: tako.internal` and an `X-Tako-Internal-Token` header matching the per-instance secret.
- **Transport**: the instance's private loopback TCP endpoint.
- **Process-exit fast path**: before each probe, Tako checks whether the process has exited. If it has, the instance is marked dead immediately — no need to wait for a probe timeout.
- **Failure threshold**: a single probe failure after the first successful probe marks the instance dead and triggers replacement. Once an instance is known healthy, we trust that any failure is real.
- **Recovery**: a single successful probe resets the failure count.

The `tako.sh` SDK implements this endpoint for you. It also validates and echoes the internal token header, so no extra wiring is required in your app.

## Scaling

`tako scale` sets the desired instance count for an app on one or more servers. The value is stored as runtime state on the server, not in `tako.toml`, so it survives deploys, rollbacks, and restarts.

```bash
tako scale 3                    # 3 instances on every production server
tako scale 0                    # Scale to zero (on-demand mode)
tako scale 2 --server la        # 2 instances on the "la" server only
```

Outside a project directory you can pass `--app` and `--env` explicitly.

### Scale-to-Zero (On-Demand Mode)

New deploys start with one hot instance per server. Opt into scale-to-zero with `tako scale 0`:

- After a deploy, one warm instance is always running so the first request after a deploy is served immediately.
- Once scaled to zero, instances stop after the configured idle timeout (default 5 minutes).
- The next request triggers a cold start. Tako spins up an instance and holds the request until it's healthy, up to a 30 second deadline.
  - If no instance becomes ready in time, the proxy returns `504 App startup timed out`.
  - If the cold start fails before readiness, the proxy returns `502 App failed to start`.
  - While a cold start is in progress, other arriving requests queue (up to 1000 by default). If that queue fills, the proxy returns `503 App startup queue is full` with a `Retry-After: 1` header.

Scale-to-zero is right for low-traffic or intermittent workloads where the occasional ~1–2s cold start is acceptable. For latency-sensitive apps, the default of one hot instance per server avoids cold starts entirely.

## Rolling Updates

When you deploy a new version against a non-zero desired count, Tako replaces instances one at a time:

1. Start a new instance on the new release.
2. Wait for its first passing health check (30s timeout).
3. Add it to the load balancer.
4. Gracefully drain the old instance (finish in-flight requests, 30s timeout).
5. Stop the old instance.
6. Repeat until all instances are replaced.
7. Flip the `current` symlink to the new release.

If a new instance fails its health check, Tako rolls back automatically: it stops the failed instance, keeps the previous ones running, and reports the failure.

When desired instances is 0, deploys still start one warm instance for the new release so the first incoming request doesn't eat a cold start.

### Deploy Lock

Each `{app}/{env}` combination on a server can only have one deploy running at a time. A second concurrent deploy for the same target fails immediately with a retry hint. The lock lives in memory — restarting `tako-server` clears it, and the interrupted deploy can simply be retried.

## TLS and Certificates

In production, Tako terminates TLS itself:

- **ACME (Let's Encrypt)** issues and renews certificates for your routes.
- **SNI-based selection** picks the right certificate during the TLS handshake, with wildcard fallback if an exact match isn't stored.
- **Renewal runs every 12 hours** and renews certificates 30 days before expiry, with zero downtime.
- **HTTP-01 challenges** happen transparently on port 80.
- **DNS-01 challenges** are supported for wildcards via [`lego`](https://go-acme.github.io/lego/), which `tako-server` downloads and installs on demand. Run `tako servers setup-wildcard` first to configure DNS provider credentials.
- **Fallback certificate**: if an SNI hostname has no cert yet, Tako serves a self-signed default so the handshake still completes and the proxy can return proper HTTP status codes (including a clean `404` for unknown hosts).
- **Private or local hostnames** (`localhost`, `*.local`, `*.test`, and friends) skip ACME entirely and get a self-signed certificate at deploy time.

### Local Dev CA

`tako dev` runs its own local Certificate Authority so you get real HTTPS on `.test` hostnames without browser warnings:

- The root CA is generated on first run and its private key is stored in the system keychain, scoped per `TAKO_HOME` so two installations don't collide.
- Leaf certificates are minted on the fly for each app hostname via SNI.
- On first run, Tako installs the root CA into the system trust store. Expect one password prompt, with a plain-language explanation before the sudo dialog appears.
- The public CA certificate lives at `{TAKO_HOME}/ca/ca.crt` — point `NODE_EXTRA_CA_CERTS` at it if you need Node to trust internal `.test` URLs.

## Edge Proxy Caching

The proxy includes a small in-memory response cache for `GET` and `HEAD` requests.

- Caching follows your app's `Cache-Control` and `Expires` headers. There is no implicit TTL — responses without explicit cache directives are not stored.
- Cache keys include both host and URI, so different hosts routing to the same path don't share entries.
- Storage is an LRU capped at 256 MiB total, with a per-object limit of 8 MiB.
- WebSocket upgrades bypass the cache.

If you want edge caching, set `Cache-Control` on the responses you want cached. If you don't, nothing is cached — which is usually what you want for dynamic content.

## Communication Protocol

The CLI and `tako-server` talk over a Unix socket at `/var/run/tako/tako.sock`. That path is actually a symlink: the running server creates a PID-specific socket (`tako-{pid}.sock`) and atomically updates the symlink when it's ready. This lets graceful reloads hand traffic over to a fresh process without clients noticing.

Every message is a small JSON object. The commands that flow over the management socket:

| Command            | Purpose                                                                 |
| ------------------ | ----------------------------------------------------------------------- |
| `hello`            | Protocol negotiation and capability discovery                           |
| `prepare_release`  | Download runtime and install production dependencies before deploy      |
| `deploy`           | Deploy a new version with routes and optional secrets                   |
| `scale`            | Change desired instance count                                           |
| `delete`           | Remove an app's state and routes                                        |
| `rollback`         | Roll back to a previous release                                         |
| `routes`           | List current route mappings                                             |
| `stop`             | Stop a running app                                                      |
| `status`           | Get status of a specific app                                            |
| `list`             | List all deployed apps with their status                                |
| `update_secrets`   | Update secrets for a deployed app (refreshes workers + rolling restart) |
| `list_releases`    | Return release/build history for an app                                 |
| `get_secrets_hash` | Get the SHA-256 hash of an app's current secrets                        |
| `server_info`      | Return server runtime config and upgrade mode                           |
| `enter_upgrading`  | Acquire the durable upgrade lock                                        |
| `exit_upgrading`   | Release the durable upgrade lock                                        |

App instances never connect to this socket — their lifecycle is driven directly by `tako-server`.

Alongside the management socket, there's a second **internal socket** at `{tako_data_dir}/internal.sock` (again, a symlink to a PID-specific file). This is the bus that apps use to enqueue workflow runs, send `signal()` events, and publish to channels without going back out over the HTTPS proxy. The SDK finds it via `TAKO_INTERNAL_SOCKET` and tags every request with `TAKO_APP_NAME`, so one socket can multiplex every deployed app on the host. The management socket rejects internal commands and vice versa — the two channels never cross.

### Instance Transport

Deployed app instances bind to `127.0.0.1` on an OS-assigned port (`PORT=0`, `HOST=127.0.0.1`). The SDK reports the bound port back to `tako-server` by writing it to file descriptor 4 once the listener is ready. From that point on, `tako-server` proxies both real traffic and health probes to the instance's loopback endpoint.

Secrets never travel as environment variables or command-line arguments. At spawn time, `tako-server` opens a pipe on file descriptor 3 and writes a single JSON envelope:

```json
{
  "token": "<per-instance internal auth token>",
  "secrets": { "DATABASE_URL": "...", "API_KEY": "..." }
}
```

The SDK reads fd 3 once, closes it, and exposes the secrets through the typed `secrets` object generated by `tako typegen`. The token authenticates `Host: tako.internal` requests such as health probes. Nothing inherits into subprocesses the app spawns, and secrets never touch disk as plaintext.

## Server Filesystem Layout

On each deployment host, Tako organizes everything under `/opt/tako/`:

```
/opt/tako/
  config.json              # Server-level config (name, DNS provider)
  tako.db                  # Persisted app state (SQLite)
  runtimes/{tool}/{version}/  # Downloaded runtime binaries
  acme/credentials.json    # ACME account credentials
  certs/{domain}/          # TLS certificates (fullchain.pem, privkey.pem)
  apps/{app}/{env}/
    current -> releases/{version}   # Active release symlink
    releases/{version}/             # Release files + app.json
    data/                           # Per-app persistent data (app/ and tako/)
    logs/                           # Persistent logs
```

Each app plus environment gets its own directory tree, so `my-app/production` and `my-app/staging` happily coexist on one machine.

Runtime binaries are downloaded directly from upstream releases using specs in the runtime plugins — no external version manager is required. Archives are checksum-verified and cached by version.

## Monitoring

`tako-server` exposes Prometheus metrics on `http://127.0.0.1:9898/` by default (the port is configurable with `--metrics-port`; set to `0` to disable). The endpoint binds to loopback only.

| Metric                                   | Type      | Description                                                               |
| ---------------------------------------- | --------- | ------------------------------------------------------------------------- |
| `tako_http_requests_total`               | Counter   | Proxied requests by status class                                          |
| `tako_http_request_duration_seconds`     | Histogram | End-to-end proxy request latency                                          |
| `tako_upstream_request_duration_seconds` | Histogram | Upstream-only latency; subtract from end-to-end to isolate proxy overhead |
| `tako_http_active_connections`           | Gauge     | Currently active connections                                              |
| `tako_cold_starts_total`                 | Counter   | Cold starts triggered                                                     |
| `tako_cold_start_duration_seconds`       | Histogram | Cold start duration distribution (success and failure)                    |
| `tako_cold_start_failures_total`         | Counter   | Cold start failures by reason (`spawn_failed`, `instance_dead`)           |
| `tako_tls_handshake_failures_total`      | Counter   | TLS handshake failures by reason (`no_sni`, `cert_missing`)               |
| `tako_instance_health`                   | Gauge     | Instance health (1=healthy, 0=unhealthy)                                  |
| `tako_instances_running`                 | Gauge     | Running instances                                                         |

Every metric carries a `server` label so multi-server deployments are distinguishable without scraper-side relabeling; per-app metrics also carry an `app` label. Only proxied requests are counted for the request/upstream histograms — ACME challenges, direct static asset responses, and unmatched `404`s are excluded. `tako_tls_handshake_failures_total` only tracks Tako-visible reasons; raw TLS protocol failures inside Pingora's listener are not counted.

Scrape with self-hosted Prometheus, Grafana Cloud, Datadog, or any Prometheus-compatible agent. For remote scraping, expose port 9898 on a private network interface (Tailscale, WireGuard, or similar).

## Workflows

Tako apps can run durable background work alongside their HTTP instances — the "backend of your backend" use case: image processing, emails, reindexing, LLM calls. It's first-class, not a separate service.

Drop a file into `workflows/` (JavaScript/TypeScript) or register handlers in a `cmd/worker/main.go` binary (Go), then enqueue from anywhere:

```ts
import sendEmail from "../workflows/send-email";
await sendEmail.enqueue({ to: "user@example.com" });
```

Each workflow module default-exports a typed handle from `defineWorkflow<P>(name, opts)`, where `opts.handler` is the workflow body. The handle's `.enqueue(payload, opts?)` method is type-checked against the declared payload `P` — no codegen step is needed for enqueue typing.

Workers run as a **separate process** from HTTP instances, so heavy workflow dependencies (image libraries, ML bindings) don't bloat the request-serving binary. Workers receive the same environment, `TAKO_DATA_DIR`, and fd-3 secrets as HTTP instances. By default a worker is scale-to-zero: it spawns on the first enqueue or cron tick, exits when idle long enough, and respawns on demand.

Use `worker: "name"` in workflow opts to assign a workflow to a named worker group. Workflows without `worker` belong to the `default` group; worker processes launched with `TAKO_WORKFLOW_WORKER=<name>` load only that group.

Features you get for free:

- **Retries with exponential backoff** (1s base, capped at 1h, ±20% jitter).
- **Delayed runs** (`runAt: new Date(...)`) and **cron schedules** declared in the `defineWorkflow` config (`schedule: "0 9 * * *"`).
- **Multi-step workflows** via `step.run("name", fn)`. Step results are checkpointed to SQLite, so a crashed run resumes from the last completed step on retry.
- **Events** via `signal(event, payload?)` — also exported from `tako.sh` — and `step.waitFor(name, opts)`. Long `step.sleep(...)` calls defer the run back into the queue instead of blocking a worker slot.
- **Graceful drain** — `tako stop` and `tako delete` wait for in-flight runs (up to 120s) before tearing down.

Queue state lives in `{tako_data_dir}/apps/<app>/runs.db` (SQLite with WAL). `tako-server` owns the database and the shared internal socket; the worker polls over that socket. Enqueues from HTTP handlers go over the same socket, so there's no external queue service, no Redis, no Postgres required.

## Channels

Channels are Tako-owned durable pub-sub streams available on one public route:

- `GET /channels/<name>` with `Accept: text/event-stream` serves Server-Sent Events.
- `GET /channels/<name>` with `Upgrade: websocket` upgrades to a WebSocket.

You declare channels as files under `channels/<name>.ts`, each default-exporting `defineChannel(config?).$messageTypes<M>()`. The filename is the wire channel name; dynamic values are typed query params declared with `paramsSchema` and validated by `tako-server` before app auth. Whether the channel is SSE or WebSocket is inferred from the config: if you define a `handler` map, the channel is bidirectional WebSocket (each client frame routes through the matching handler and its return value fans out to subscribers). If there's no `handler`, the channel is broadcast-only SSE.

```ts
// channels/chat.ts
import { defineChannel } from "tako.sh";

type ChatMessages = {
  msg: { text: string; userId: string };
  typing: { userId: string };
};

export default defineChannel({
  paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
  auth: {
    headerName: "authorization",
    async verify(input) {
      const session = await readSession(input.header);
      return session ? { subject: session.userId } : false;
    },
  },
  handler: {
    msg: async (data, ctx) => {
      await db.saveMessage(ctx.params.roomId, data);
      return data;
    },
    typing: async (data) => data,
  },
}).$messageTypes<ChatMessages>();
```

Channels keep a bounded replay window so reconnects and `tako-server` reloads don't drop clients mid-conversation. SSE clients resume from `Last-Event-ID`; WebSocket clients resume from `last_message_id` in the query string or the first `tako.auth` frame. If the requested cursor is older than the retained window, Tako returns `410 Gone` and the client can resubscribe from the tail.

Channel routes are flat and exact: `channels/chat.ts` maps to `/channels/chat`, and params are query strings such as `/channels/chat?roomId=room-123`.

Server-side publishing goes through the channel module directly (`await missionLog({ base }).publish(...)`) — it never round-trips through the HTTPS proxy. That's what the internal socket is for.

## What to Read Next

- [CLI reference](/docs/cli) — every command, flag, and prompt.
- [`tako.toml` reference](/docs/tako-toml) — the full configuration schema.
- [Presets](/docs/presets) — framework presets and runtime-local overrides.
- [Deployment guide](/docs/deployment) — server setup, TLS, wildcards, monitoring.
- [Development guide](/docs/development) — working with `tako dev`, LAN mode, variants.
- [Troubleshooting](/docs/troubleshooting) — common issues and how to fix them.
