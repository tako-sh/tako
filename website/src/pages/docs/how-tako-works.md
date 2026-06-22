---
layout: ../../layouts/DocsLayout.astro
title: "How Tako Works - Tako Docs"
heading: "How Tako Works"
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako is a local CLI plus a server runtime. The CLI owns project config, local development, builds, deploy orchestration, generated files, secrets, credentials, server inventory, and operational commands. `tako-server` runs on your hosts and owns routing, TLS, app processes, rolling updates, scale-to-zero, logs, backups, durable channels, workflow workers, image optimization, and signed remote management.

## Main Pieces

| Piece         | Role                                                                                                                                                      |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tako`        | Local CLI for init, dev, deploy, logs, releases, scale, delete, servers, secrets, credentials, storage, backups, and generated files.                     |
| `tako-server` | Remote runtime with a Pingora proxy, TLS, app supervision, state storage, backups, logs, metrics, and management APIs.                                    |
| SDKs          | Runtime helpers for JavaScript/TypeScript (`tako.sh`), Go (`tako.sh`), and Rust (`tako`).                                                                 |
| `tako.toml`   | App identity, runtime, routes, environments, builds, storage bindings, backups, SSL, source-IP policy, workflows, and server targets.                     |
| Presets       | Framework defaults for entrypoints, asset roots, and dev commands. Built-ins currently include Vite, TanStack Start, and Next.js for JavaScript runtimes. |

## Local Development

`tako dev` registers the selected config file with a persistent local daemon. The daemon serves your app through trusted HTTPS on `.test` and `.tako.test` hostnames, watches Tako config and generated-file inputs, streams logs, and keeps enough state to attach another CLI session later.

On macOS, Tako uses split DNS plus a launchd-managed loopback proxy so routes use normal `https://app.test/` URLs. On Linux, Tako uses a loopback alias and system routing rules for portless HTTPS when supported. The underlying HTTPS daemon listens on `127.0.0.1:47831`.

Development routes come from `[envs.development].route` or `[envs.development].routes` when present. If you configure managed `.test` or `.tako.test` routes, they replace the default `{app}.test` host. External development routes are additional aliases and must be pointed at the dev proxy by you.

The app starts immediately when `tako dev` starts, goes idle after 30 minutes without an attached CLI client, and wakes on the next request. `Ctrl-C` unregisters the app and removes routes. Pressing `b` backgrounds the app in the daemon. Interactive sessions can toggle LAN `.local` aliases with `l` and temporary public tunnel URLs with `t`; tunnel hostnames are stable for the same app and Tako Identity.

## Deploy Flow

Deploys build locally and roll out on every mapped server:

1. Validate config, routes, server targets, secrets, storage credentials, SSL credentials, workflow/channel storage requirements, and backup settings.
2. Resolve the source root, app subdirectory, runtime, preset, entrypoint, assets, variables, and release command.
3. Build in `.tako/build`, respecting `.gitignore`, preserving symlinks, and excluding `node_modules`, `.git`, `.tako`, and `.env*` from artifacts.
4. Upload the target artifact over signed private HTTP management.
5. Ask each server to extract and prepare the release.
6. Run the configured `release` command once on the leader server, if present.
7. Roll new instances into traffic and drain old instances.
8. Finalize `current`, prune old or excess releases, and create a post-deploy backup when backups are enabled.

Native releases run from the prepared artifact. Container releases upload source, then the server builds the configured container file with Podman and runs the image on a private loopback port.

## Runtime Model

Native HTTP instances bind `HOST=127.0.0.1` and `PORT=0`. The SDK binds an OS-assigned loopback port and reports it on fd 4. `tako-server` then probes the private endpoint and routes traffic through the app load balancer.

Container HTTP instances receive `HOST=0.0.0.0`, `PORT=3000`, and `TAKO_BOOTSTRAP_DATA`. They do not receive fd 3, fd 4, the internal socket, or `TAKO_DATA_DIR` in v0. A configured container workflow `run` starts a separate process from the same image with the internal socket mounted.

Secrets and storage bindings are stored encrypted on the server. Native app and worker processes receive them through the fd 3 bootstrap envelope. Containers receive the same envelope through `TAKO_BOOTSTRAP_DATA`. Backup storage is not exposed to app code unless it is also configured as an app storage binding.

## Routing

Routes are declared per environment and can be exact hosts, wildcard hosts, host-plus-path routes, or wildcard-plus-path routes. Tako chooses the most specific match, serves static files from the deployed `public/` directory when possible, and otherwise proxies to an app instance. Route conflicts are rejected during deploy.

Public `/_tako/*` paths are reserved after a request matches an app route:

- `/_tako/channels/<name>` serves durable SSE or WebSocket channels.
- `/_tako/image` serves the public image optimizer.
- `/_tako/storages/<binding>/<key>` serves signed local storage upload/download routes.

Unknown production routes return `404`. Unknown managed local development hosts return a helpful `421` that lists registered dev routes.

## TLS And Source IP

Public routes use Let's Encrypt by default. Exact Let's Encrypt routes use HTTP-01 unless the environment has `ssl.cloudflare`, in which case they use Cloudflare DNS-01. Wildcard Let's Encrypt routes always require `ssl.cloudflare`. `ssl = "cloudflare"` uses Cloudflare Origin CA certificates instead.

`source_ip` controls client-IP derivation per environment. `auto` detects Cloudflare when possible, then configured trusted proxies, then direct peers. `direct`, `cloudflare-proxy`, and `trusted-proxy` provide stricter modes.

## Scaling And Health

Desired instance count is server-side runtime state, not `tako.toml` config. New deploys start with one desired instance. `tako scale` changes the persisted count per server, and the value survives restarts, deploys, and rollbacks.

Scaling to zero enables on-demand cold starts. Deploy still keeps one warm instance immediately after rollout so the app is reachable, then idle timeout can stop it later. A cold request waits for readiness, queues behind an in-progress cold start up to the server limit, and gets a generic 502/503/504 response if startup fails.

Health checks call `/status` with `Host: <app>.tako` and `X-Tako-Internal-Token`. SDK adapters handle this endpoint and echo the token. Browser-facing production 5xx bodies stay generic; detailed startup, proxy, storage, channel, and image diagnostics go to app logs.

## Data And Backups

Each deployed app has durable data under the server data directory:

```text
/opt/tako/apps/{app}/{env}/data/
├── app/   # exposed to app code as TAKO_DATA_DIR for native releases
└── tako/  # Tako-owned workflow/channel/cache state
```

Backups are opt-in with `[envs.<env>].backup`. Backup storage must be a private S3-compatible storage resource. Archives are compressed, encrypted with environment-managed backup keys, uploaded under Tako's reserved backup prefix, and indexed remotely. Deploy creates a backup after successful finalize when backups are enabled; the server also runs due backups about every 24 hours.

Workflow state is backed up. Channel replay and SDK cache storage are not backed up because they are transient and recomputable.

## Remote Management

Normal app operations use signed HTTP management over the server's private Tailscale address. SSH is used for setup, repair, and server maintenance. `hello` and `server_info` are public probes; all mutating commands require a signed request from an enrolled SSH key.

Artifact uploads and log reads use dedicated signed HTTP endpoints. App-scoped commands such as deploy, status, logs, scale, releases, backups, delete, and secrets sync do not use SSH for the normal path.

## Observability

`tako logs` reads app stdout/stderr plus app-scoped server diagnostics. Global `--json` emits structured stdout for agents and automation: history mode returns one object with a `logs` array, while `--tail --json` emits JSONL events. `tako-server` also exposes Prometheus metrics on localhost by default at port `9898`, including request counts, latency, active connections, cold starts, TLS handshake failures, and instance health.
