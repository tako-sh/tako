---
title: "How Tako Works"
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

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

1. Validate config, routes, server targets, secrets, storage credentials, SSL credentials, channel/presence/store/workflow storage requirements, and backup settings.
2. Resolve the source root, app subdirectory, runtime, preset, entrypoint, assets, variables, and release command.
3. Build in `.tako/build`, respecting `.gitignore`, omitting symlinks, and excluding `node_modules`, `.git`, `.tako`, and `.env*` from artifacts.
4. Upload the target artifact over signed private HTTP management.
5. Ask each server to extract and prepare the release.
6. Run the configured `release` command once on the leader server, if present.
7. Roll new instances into traffic and drain old instances.
8. Finalize `current`, prune old or excess releases, and create a post-deploy backup when backups are enabled.

Native releases run from the prepared artifact. Container releases upload source, then the server builds the configured container file with Podman and runs the image on a private loopback port.

## Runtime Model

Native HTTP instances bind `HOST=127.0.0.1` and `PORT=0`. The SDK binds an OS-assigned loopback port and reports it on fd 4. `tako-server` then probes the private endpoint and routes traffic through the app load balancer.

Container HTTP instances receive `HOST=0.0.0.0`, `PORT=3000`, and `TAKO_BOOTSTRAP_DATA`. The app data directory is mounted into the container at `container_options.data_dir` (`/tako/data` by default), and `TAKO_DATA_DIR` names that container path. They do not receive fd 3, fd 4, or the internal socket in v0. A configured container workflow `run` starts a separate process from the same image with the same data mount and `TAKO_DATA_DIR`, plus the internal socket; container releases support one configured workflow `run` command in v0.

Secrets and storage bindings are stored encrypted on the server. Native app and worker processes receive them through the fd 3 bootstrap envelope. Containers receive the same envelope through `TAKO_BOOTSTRAP_DATA`. Backup storage is not exposed to app code unless it is also configured as an app storage binding.

## Routing

Routes are declared per environment and can be exact hosts, wildcard hosts, host-plus-path routes, or wildcard-plus-path routes. Tako chooses the most specific match, serves static files from the deployed `public/` directory when possible, and otherwise proxies to an app instance. Static responses carry an `ETag` and revalidate with `304 Not Modified`. Route conflicts are rejected during deploy.

Public `/_tako/*` paths are reserved after a request matches an app route:

- `/_tako/channels/ws` serves shared WebSocket channels and Presence; `/_tako/channels/ws/<name>` serves dedicated WebSockets, and `/_tako/channels/sse/<name>` serves dedicated SSE streams.
- `/_tako/image` serves the public image optimizer.
- `/_tako/storages/<binding>/<key>` is served directly by the Rust proxy for signed local storage uploads and downloads, independently of app workers. The SDK creates URLs and does not perform storage file I/O.

Channels are broadcast streams for app events. All authorized subscribers to the same channel and params can read the same messages, each subscriber keeps its own cursor, and receiving a message does not remove it for other subscribers.

Unknown production routes return `404`. Unknown managed local development hosts return a helpful `421` that lists registered dev routes.

The production proxy accepts request bodies up to 128 MiB. Channel WebSocket frame payloads use the same limit. Each client IP may have 2,048 active requests by default; operators can override that ceiling with `TAKO_MAX_REQUESTS_PER_IP`.

## TLS And Source IP

Public routes use Let's Encrypt by default. Exact Let's Encrypt routes use HTTP-01 unless the environment has `ssl.cloudflare`, in which case they use Cloudflare DNS-01. Wildcard Let's Encrypt routes always require `ssl.cloudflare`. `ssl = "cloudflare"` uses Cloudflare Origin CA certificates instead.

`source_ip` controls client-IP derivation per environment. `auto` detects Cloudflare when possible, then configured trusted proxies, then direct peers. `direct`, `cloudflare-proxy`, and `trusted-proxy` provide stricter modes.

## Scaling And Health

Desired instance count is server-side runtime state, not `tako.toml` config. New deploys start with one desired instance. `tako scale` changes the persisted count per server, and the value survives restarts, deploys, and rollbacks.

Scaling to zero enables on-demand cold starts. Deploy still keeps one warm instance immediately after rollout so the app is reachable, then idle timeout can stop it later. A cold request waits for readiness, queues behind an in-progress cold start up to the server limit, and gets a generic 502/503/504 response if startup fails.

If an instance exits or fails its health checks, the server replaces it. The first loss is replaced at once; repeated losses wait with jittered backoff from about 1 second doubling to 30 seconds. A periodic check every few seconds starts any instances still missing below the desired count, so a failed or postponed replacement is retried without a deploy. The backoff resets after the app stays healthy for a minute, and a successful deploy or `tako scale` clears it. Apps scaled to zero are started only by requests.

During rolling deploys, new instances do not receive public traffic and old instances are not drained until every replacement batch stays healthy for a short stability window. If active health probing marks a new batch unhealthy during that window, deploy rolls back and keeps the previous release serving.

Draining an instance, for a deploy or a scale-down, stops new requests to it and waits up to 30 seconds for its in-flight requests to finish, server-sent event streams included. Connections the app upgraded (it answered `101 Switching Protocols`, as for a WebSocket) do not hold the drain: once the other requests finish, the server sends `SIGTERM` and kills the process if it has not exited after a grace period (15 seconds for native processes). Sockets still open to that instance are closed, and clients should reconnect. On `SIGTERM` the JavaScript SDK lets in-flight requests and streams finish for up to 8 seconds, then closes the rest abruptly so clients see an error rather than a response that looks complete; an app that serves its own WebSockets can close them with code `1012` (service restart). Long-lived subscriptions belong on channels and presence: those connections are served by `tako-server`, not by app instances, and stay open across deploys. Idle scale-down still counts every open connection, so an instance with an open socket is never treated as idle.

Health checks call `/status` with `Host: <app>.tako` and `X-Tako-Internal-Token`. SDK adapters handle this endpoint and echo the token. Browser-facing production 5xx bodies stay generic; detailed startup, proxy, storage, channel, and image diagnostics go to app logs.

## Data And Backups

Each deployed app has durable data under the server data directory:

```text
/opt/tako/apps/{app}/{env}/data/
├── app/   # exposed to app code as TAKO_DATA_DIR; mounted into containers
└── tako/  # Tako-owned workflow/channel/cache state
```

`TAKO_SHARED_DATA_DIR` exposes `/opt/tako/shared/data/app` to apps on this server. Per-app users retain private directories and use a common group for shared files. Local development shares a directory under the same `TAKO_HOME`. Standalone containers and Compose services receive `/tako/shared-data` as a managed mount.

These directories hold plain files. Tako does not discover or capture databases in them.

### Hosted databases

A [hosted database](/databases/) is a SQLite file under `databases/<id>/` in the server data directory of one server, owned by `tako-server` rather than by an app user. The server serves it to apps on `127.0.0.1:9845` over the libSQL HTTP protocol. Each app binding gets its own token in the SDK bootstrap envelope; a token resolves to one database for one app environment. Connections enable foreign keys, run in WAL mode with `synchronous = FULL`, and refuse `ATTACH`, `VACUUM INTO`, extension loading, and persistent `PRAGMA` writes other than `user_version`, `application_id`, and `auto_vacuum`.

Each database's record (name, server, access, and snapshot encryption key) lives in the network's encrypted shared state, readable by administrators. A database with `access = "network"` can be bound by apps on other servers: their own server forwards requests over the private WireGuard network to the database's server on port 9847, identifying the app environment rather than sending its token. The database's server accepts forwarded requests only from network servers and only for app environments an administrator's deploy granted.

With `[databases.<name>].backup`, the server snapshots a changed database hourly, before release commands, and before restores or imports: a `VACUUM INTO` copy, integrity-checked, zstd-compressed, and AES-256-GCM encrypted. Replacing a database (restore or import) pauses its requests with `503` responses for the swap.

### App data archives

Full app-directory archives are manual (`tako backups now`) and opt-in with `[envs.<env>].backup`. They include local workflow state; channel replay and SDK cache are excluded because they are transient. Shared-directory files, Compose named volumes, external databases, and S3 contents need separate protection. See [Backups](/backups/) for recovery and boundaries.

## Remote Management

Normal app operations use signed, pinned HTTPS authenticated by the enrolled device's identity and network permissions. SSH is used for bootstrap, recovery, and server maintenance. WireGuard carries private network traffic; deploys do not require a workstation WireGuard interface.

Artifact uploads and log reads use dedicated signed HTTP endpoints. App-scoped commands such as deploy, status, logs, scale, releases, backups, delete, and secrets sync do not use SSH for the normal path.

## Observability

`tako logs` reads app stdout/stderr plus app-scoped server diagnostics. Global `--json` emits structured stdout for agents and automation: history mode returns one object with a `logs` array, while `--tail --json` emits JSONL events. `tako-server` also exposes Prometheus metrics on localhost by default at port `9898`, including request counts, latency, active connections, cold starts, TLS handshake failures, and instance health.
