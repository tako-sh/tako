---
layout: ../../layouts/DocsLayout.astro
title: "How Tako Works - Tako Docs"
heading: "How Tako Works"
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako pairs a local CLI with a server runtime you run on your own hosts. The CLI owns project config, local development, builds, deploy orchestration, generated files, secrets, credentials, and server inventory. `tako-server` owns public routing, TLS, process supervision, rolling updates, scale-to-zero, durable channels, workflow workers, image optimization, app data, and remote management.

## Main Pieces

| Piece         | Role                                                                                                                                    |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `tako`        | CLI for init, dev, deploy, servers, secrets, credentials, storage, backups, logs, releases, scale, delete, and generation.              |
| `tako-server` | Remote runtime with proxying, TLS, app supervision, local state, backups, logs, metrics, and management APIs.                           |
| `tako.sh`     | JavaScript/TypeScript SDK for runtime state, secrets, storage, channels, workflows, images, and framework adapters.                     |
| Presets       | Framework defaults for entrypoints, asset roots, and dev commands. Built-ins currently include Vite, TanStack Start, and Next.js.       |
| `tako.toml`   | App config for identity, routes, environments, builds, storage, backups, source-IP policy, SSL provider, workflows, and server targets. |

## Local Development

`tako dev` is a client for a persistent local daemon. It prepares trusted HTTPS, local DNS, app routes, secrets, storage bindings, generated types, fd-3 bootstrap data, and fd-4 readiness.

On macOS, Tako uses a launchd-managed loopback proxy so app URLs stay on normal HTTPS and HTTP ports:

```text
127.77.0.1:443 -> 127.0.0.1:47831
127.77.0.1:80  -> 127.0.0.1:47830
```

On Linux, Tako uses the same dedicated loopback alias with iptables redirects for `443 -> 47831`, `80 -> 47830`, and `53 -> 53535`. On NixOS, it prints a `configuration.nix` snippet instead of applying imperative setup.

If no development routes are configured, the default route is `{app}.test`. Configured `.test` and `.tako.test` routes replace that default. External development hostnames are additive when no managed local route is configured. LAN mode adds `.local` aliases for managed local routes.

The dev runtime mirrors production closely: HTTPS, real hostnames, fd-3 secrets/storage bootstrap, workflow workers, durable channels, app data directories, and public image routes all exist locally.

## Deploy Flow

A deploy builds locally and ships a prepared artifact to each server:

1. Validate config, routes, target servers, secrets, storage credentials, backup storage, provider credentials, and server target metadata.
2. Resolve the source root from git when available, otherwise from the app directory.
3. Resolve runtime, package manager, preset, `main`, assets, build stages, and version metadata.
4. Copy sources into `.tako/build`, respecting `.gitignore`, force-excluding `.git/`, `.tako/`, `.env*`, and `node_modules/`.
5. Run build stages in order, merge configured assets into `public/`, and write `app.json`.
6. Package a target-specific artifact and reuse the local artifact cache when inputs match.
7. Upload over signed private HTTP management.
8. Prepare the release on each server and run production install there.
9. Run the optional `release` command once on the leader server.
10. Roll new instances into traffic, finalize `current`, prune old releases, and create a post-deploy backup when enabled.

Deploy archives source symlinks as symlinks instead of following directory symlinks. The version is the clean git commit, the commit plus source hash when dirty, or `nogit_<hash>` outside git.

## Remote Management

Normal server installs require Tailscale for private management. The installer binds the HTTP management listener to the server's Tailscale address on port `9844`.

| Endpoint                 | Purpose                                                                    |
| ------------------------ | -------------------------------------------------------------------------- |
| `POST /rpc`              | JSON management commands. `hello` and `server_info` are public probes.     |
| `POST /release-artifact` | Signed deploy artifact upload with declared size and SHA-256 verification. |
| `POST /logs`             | Bounded historical log reads and streaming offsets.                        |

All non-probe management calls are signed with an enrolled SSH key. Requests include the key fingerprint, timestamp, nonce, and signature over the request body plus Tako's management-auth namespace. Replayed nonces and stale timestamps are rejected.

App/runtime commands such as deploy, logs, scale, releases, backups, delete, and `secrets sync` use signed HTTP management. SSH remains for setup, recovery, reload, upgrade, and uninstall flows.

## Routing And HTTPS

Routes are host patterns with optional paths. After a route matches, Tako reserves `/_tako/*` for public runtime endpoints:

| Path                              | Purpose                                  |
| --------------------------------- | ---------------------------------------- |
| `/_tako/channels/<name>`          | Durable channel SSE/WebSocket endpoints. |
| `/_tako/image`                    | Public optimized images.                 |
| `/_tako/storages/<binding>/<key>` | Signed local-storage GET/PUT routes.     |

Tako terminates TLS on `tako-server`. Public exact routes use Let's Encrypt HTTP-01 by default. Public wildcard routes use Let's Encrypt DNS-01 with Cloudflare credentials. `ssl = "cloudflare"` uses Cloudflare Origin CA and also requires encrypted `ssl.cloudflare` credentials.

HTTP redirects to HTTPS by default, except `/.well-known/acme-challenge/*`. Forwarded HTTPS metadata is trusted only from loopback peers, Cloudflare peers, or peers listed in `trusted_proxy.trusted_cidrs`; direct clients cannot spoof `X-Forwarded-Proto` or `Forwarded: proto=https` to bypass redirects.

## Source IPs

`source_ip` is selected per environment:

```toml
[envs.production]
route = "app.example.com"
source_ip = "direct"
```

Generated configs omit `source_ip`, which behaves like `auto`: Cloudflare peers can supply `CF-Connecting-IP`, configured trusted proxy peers can supply configured client-IP headers, and all other requests use the direct TCP peer IP.

Use `cloudflare-proxy` when all traffic must arrive through Cloudflare. Use `trusted-proxy` for a configured front proxy. Use `direct` to ignore proxy headers.

The resolved client IP is also used for the per-IP active request limit. The default is 2048 active requests per client IP; `TAKO_MAX_REQUESTS_PER_IP` can override it for controlled benchmarks or deliberately tuned deployments.

## Process Model

New deploys start with one desired instance per server. `tako scale` changes that desired count and persists it across restarts, deploys, and rollbacks. Each app also has an effective server maximum, defaulting to two app instances per available CPU for new deploys; explicit scale requests above that maximum fail. Scaling to zero keeps the app registered; the first request triggers a cold start.

Rolling update happens one server at a time inside each server: start a new instance, wait for health, add it to the load balancer, drain an old instance, repeat, then update `current`.

Health probes call `Host: <app>.tako` on `/status`. The JS SDK supplies this internal status response before user routing. Go apps use the SDK status handling. Production browser-facing 5xx responses stay generic while detailed diagnostics go to app logs.

## Secrets, Storage, And Backups

Project secrets, storage credentials, provider credentials, and backup keys are encrypted in `.tako/secrets.json`. Expiry dates are plaintext metadata so deploy can fail on expired selected credentials and warn for credentials expiring within 30 days.

Server-side secrets and storage bindings are stored encrypted in SQLite. Fresh HTTP instances and workflow workers receive them through fd 3 at spawn time rather than through inherited service environment variables.

App storage bindings are declared under `[envs.<env>].storages` and exposed as `tako.storages.<name>`. Backup storage is separate unless the same resource is also listed as an app storage binding.

Backups capture app-owned data and durable workflow state. Transient channel replay storage is excluded. Archives are compressed, encrypted, uploaded under `_tako/backups/{app}/{env}/{server}/`, indexed remotely, and retained for 30 days by default.

## Workflows And Channels

JavaScript channels live in `<app_root>/channels/*.ts` and export `defineChannel(...)`. Routes are exact and flat under `/_tako/channels/<name>`.

JavaScript workflows live in `<app_root>/workflows/*.ts` and export `defineWorkflow(...)`. Workflow workers can be always-on or scale-to-zero. The default is `workers = 0`, so the server starts a worker on enqueue or cron tick and stops it after an idle window.

Workflow state is durable in SQLite. `ctx.run`, `ctx.sleep`, `ctx.waitFor`, retries, unique enqueue keys, schedules, and signals survive process restarts.

## Observability

Logs are app-scoped and include app stdout/stderr plus server diagnostics for startup, proxying, channels, image transforms, backups, workflows, and static file serving. `tako logs` reads them over signed HTTP management.

`tako-server` exposes Prometheus metrics on localhost port `9898` by default. Metrics include request counts and durations, instance health, cold starts, deploys, TLS events, log drops, channels, workflows, and image worker activity. Use `--metrics-port 0` to disable request/upstream metrics collection and the endpoint.
