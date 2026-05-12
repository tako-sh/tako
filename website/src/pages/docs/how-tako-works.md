---
layout: ../../layouts/DocsLayout.astro
title: "How Tako works: rolling deploys, TLS, health checks, and scale to zero - Tako Docs"
heading: "How Tako Works"
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako gives one project a local HTTPS development loop and a self-hosted production runtime. The local CLI owns configuration, builds, deploy orchestration, secrets, and server setup. `tako-server` runs on your hosts and owns routing, TLS, app processes, workflow workers, channels, image optimization, health checks, and rolling updates.

The protocol is v0. App behavior is intentionally direct: runtime behavior lives in runtime plugins, framework presets only supply metadata, and every deployed environment is identified on a server as `{app}/{env}`.

## Main Pieces

### CLI

The `tako` CLI reads `tako.toml` from the current directory by default. Pass `-c` or `--config <CONFIG>` to select another config file; the selected file's parent directory is the app directory.

The CLI:

- initializes projects with `tako init`
- runs local HTTPS development with `tako dev`
- builds and uploads deploy artifacts with `tako deploy`
- manages local encrypted secrets
- registers servers and upgrades `tako-server`
- queries logs, status, releases, and scale settings

Status and deploy operations use signed remote management over Tailscale-reachable HTTP. SSH is still used for setup, upload, install, recovery, and log file access.

### Runtime Plugins

Runtime plugins define how an app builds, installs production dependencies, starts, and sets runtime environment variables. Built-in runtimes are Bun, Node, and Go.

JavaScript runtimes start through SDK entrypoints. Go apps compile to a native binary and run directly.

### Presets

Presets are framework metadata. They can provide:

- `main`
- `assets`
- `dev`

They do not define production start commands, install commands, runtime downloads, or build behavior. Official preset manifests live in `presets/javascript.toml` and `presets/go.toml`.

## Local Development

`tako dev` starts a persistent local daemon and registers the selected config file as an app session.

Default route:

```text
https://{app}.test/
```

On macOS, Tako installs a launchd-managed loopback proxy for portless HTTPS on `127.77.0.1:80` and `127.77.0.1:443`. On Linux, Tako configures local routing for the same loopback address. Both platforms use a local DNS listener for `.test` and `.tako.test`.

The dev daemon starts the app immediately. If no CLI client stays attached for 30 minutes, the process can go idle while routes remain registered. The next request wakes the app and waits for readiness.

Interactive `tako dev` streams logs and status in the terminal. Press:

- `r` to restart
- `l` to toggle LAN mode
- `b` to leave the app running in the background
- `Ctrl-C` to stop and unregister it

LAN mode advertises concrete `.local` aliases with mDNS. Wildcard routes can still match in the proxy, but mDNS cannot advertise wildcard names.

## Deploy Flow

`tako deploy` targets `production` unless `--env` is provided. The target environment must exist in `tako.toml` and define `route` or `routes`; `development` is reserved for `tako dev`.

The deploy pipeline is:

1. Validate config, routes, secrets, and server target metadata.
2. Resolve source root: git root when available, otherwise the app directory.
3. Copy the source tree into `.tako/build`, respecting `.gitignore`.
4. Link `node_modules` from the original tree into the build workspace.
5. Resolve preset, runtime, `main`, build stages, assets, and runtime version.
6. Run build stages.
7. Write `app.json` with runtime metadata, non-secret env vars, app subdirectory, install directory, release metadata, and idle timeout.
8. Archive the build output while excluding `.git`, `.tako`, `.env*`, and `node_modules`.
9. Upload the artifact to every selected server.
10. Extract the artifact and run production install on the server.
11. Run the optional `release` command on the leader server.
12. Perform a rolling update on each server.

Production deploy confirmation is only required for interactive production deploys where the environment was implicit. Passing `--env production` or `--yes` makes the target explicit and skips the confirmation.

## Routing

Routes are configured per environment:

```toml
[envs.production]
routes = [
  "example.com",
  "*.example.com/admin/*",
  "example.com/api/*",
]
```

The proxy matches by host and optional path. Exact hosts beat wildcards, and longer paths beat shorter paths. After a route matches, Tako reserves `/_tako/*` for platform endpoints such as channels and signed image optimization.

Static files are served from `public/` when a matching path with a file extension exists. Everything else is proxied to a healthy app instance.

## Process Model

App instances bind to private TCP on loopback. Tako sets:

- `PORT=0`
- `HOST=127.0.0.1`
- `TAKO_APP_NAME`
- `TAKO_INTERNAL_SOCKET`
- `TAKO_APP_ROOT` for JavaScript apps
- `ENV`
- `TAKO_DATA_DIR`
- runtime env vars such as `NODE_ENV` and `BUN_ENV`
- `TAKO_BUILD` on deploys

The SDK binds an OS-assigned port and reports it to Tako on fd 4. `tako-server` then routes traffic to that private endpoint.

In production, app and worker processes start from a cleared service environment. Tako preserves only minimal process env (`PATH` and `HOME` when available), then applies app/runtime vars. Secrets, internal auth tokens, and image signing material arrive through fd 3 as a bootstrap JSON envelope, not through env vars.

## Health And Scale

Active HTTP probes are the health source of truth. Probes use:

- private TCP transport
- `Host: <app>.tako`
- `/status` by default
- the per-instance internal auth token

Instances become healthy only after fd-4 readiness and successful probing. During startup, probes run at a faster tier; steady-state probes run every second. Failed probes mark instances unhealthy and eventually dead, which triggers replacement.

Each deployed app stores a desired instance count per server. New deploys default to `1`. Use `tako scale` to change it:

```bash
tako scale 2 --env production
tako scale 0 --env production
```

Desired count `0` enables scale-to-zero. Deploy still starts one warm instance so the new release is reachable immediately; idle instances stop after the environment's `idle_timeout`.

## Rolling Updates

Rolling update replaces instances one at a time:

1. Start a new instance.
2. Wait for readiness and health.
3. Add it to the load balancer.
4. Drain and stop an old instance.
5. Repeat until the release is fully active.
6. Move the server's `current` symlink to the new release.

If startup fails, Tako keeps old instances serving and reports the deploy error.

## TLS

Production TLS uses SNI. Exact certificates are used before wildcard certificates. If no valid certificate exists, Tako serves a self-signed fallback certificate while issuing or renewing certificates.

ACME HTTP-01 handles exact hostnames. Wildcard hostnames require DNS-01 through lego. Configure DNS credentials with:

```bash
tako servers setup-wildcard
```

The command currently applies DNS configuration to all configured servers.

## Workflows And Channels

Workflows are durable background runs stored in per-app SQLite under the app data directory. `tako-server` owns the database, cron ticker, leases, retries, and worker supervision. SDKs talk to the internal socket; they do not open SQLite directly.

Workers are separate from HTTP instances. They can run scale-to-zero by default and wake on enqueue or cron ticks.

JavaScript workflow files live under `<app_root>/workflows/`; JavaScript channel files live under `<app_root>/channels/`. `app_root` is configured in `tako.toml` and defaults to `src`.

Channels are durable pub-sub streams under:

```text
/_tako/channels/<name>
```

SSE channels are broadcast-only. WebSocket channels can accept client frames, run app-defined handlers, and fan out returned messages.

## Image Optimization

Server-side JavaScript can call `createImageUrl(source, opts?)` from `tako.sh` to create signed image URLs under:

```text
/_tako/image/v1/<payload>.<signature>
```

Private AVIF URLs are the default. Public URLs require `public: true`. The optimizer verifies signatures before reading sources, rejects unsafe remote sources, uses libvips, avoids upscaling, strips source metadata from transformed output, and supports JPEG, PNG, WebP, and AVIF inputs. If resize or encode work fails after a verified image source has loaded, Tako serves the original image bytes instead.

## State

On servers, runtime data lives under `/opt/tako` by default. Each deployed app has:

- releases under `/opt/tako/apps/{app}/{env}/releases/`
- a `current` symlink
- app-owned data exposed as `TAKO_DATA_DIR`
- Tako-owned internal state

App config and routes are persisted in SQLite so they survive reloads and restarts. Secrets are encrypted in server state and delivered only to fresh processes through fd 3.
