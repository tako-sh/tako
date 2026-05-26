---
layout: ../../layouts/DocsLayout.astro
title: "How Tako Works - Tako Docs"
heading: "How Tako Works"
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako is a local CLI paired with a self-hosted server runtime. The CLI owns project config, local development, builds, deploy orchestration, secrets, generated files, and server inventory. `tako-server` runs on your hosts and owns routing, TLS, process supervision, workflow workers, durable channels, image optimization, health checks, scale-to-zero, and rolling updates.

The protocol is still v0. Runtime behavior lives in runtime plugins; presets stay small and only describe framework defaults such as entrypoints, asset roots, and dev commands.

## Main Pieces

| Piece               | Role                                                                                                                        |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `tako`              | CLI for init, dev, deploy, server management, secrets, storage, backups, logs, releases, scaling, and code generation.      |
| `tako-server`       | Remote runtime with the proxy, TLS, supervisor, state store, workflow manager, image worker, and management API.            |
| `tako.sh`           | JavaScript/TypeScript SDK for fetch handlers, readiness, status, channels, workflows, storage, images, and generated types. |
| `tako.sh` Go module | Go SDK for `net/http` handlers, readiness, health checks, secrets, channels, and workflow RPCs.                             |
| Runtime plugins     | Built-in runtime definitions for Bun, Node, and Go.                                                                         |
| Presets             | Framework defaults for Vite, TanStack Start, Next.js, and future framework manifests.                                       |

## Project Identity

Each deployed app environment is identified as:

```text
{app}/{env}
```

The app name comes from `name` in `tako.toml`. If it is omitted, Tako derives it from the selected config file's parent directory. Setting `name` keeps the server-side identity stable when a directory is renamed.

The same physical server can host multiple environments of the same app because every environment gets a separate identity and filesystem tree.

## Deploy Data Flow

`tako deploy` targets `production` unless `--env` is passed. The target environment must exist, must define `route` or `routes`, and cannot be `development`.

The deploy flow is:

1. Validate config, routes, target servers, secrets, storage credentials, backup storage credentials, provider credentials when needed, and server target metadata.
2. Resolve runtime, preset, package manager, entrypoint, asset roots, build commands, and runtime version.
3. Copy the source bundle into a temporary build workspace and run local build steps.
4. Merge assets, write `app.json`, verify the resolved `main`, and package the artifact.
5. Upload the artifact to each server over signed HTTP.
6. Ask each server to prepare the release, install production dependencies, and download runtimes when needed.
7. Run the optional release command once on the leader server.
8. Sync routes, source-IP mode, secrets, storage bindings, backup binding and backup keys, and SSL bindings through remote management.
9. Start healthy new instances, add them to traffic, then drain old instances.
10. Finalize the release and create a post-deploy backup when backups are enabled.

Servers receive prebuilt artifacts. App build steps do not run on the server.

## Artifact Packaging

Deploy bundles from the git root when available, otherwise from the selected app directory. The selected config file's parent directory becomes the app subdirectory inside that source bundle.

Artifacts always exclude:

- `.git/`
- `.tako/`
- `.env*`
- `node_modules/`

Additional exclusions come from `[build].exclude`, per-stage `exclude`, and `.gitignore`. Source and build archives preserve symlinks as symlinks instead of following them, so directory symlinks do not expand outside the project. Source hashes include symlink targets, which means changing a link invalidates the artifact cache.

## Remote Management

Normal installs require Tailscale for remote management. The installer binds a private HTTP management listener to the server's Tailscale address on port `9844`.

The management API uses the same typed `Command -> Response` protocol as the local Unix socket:

| Endpoint                 | Purpose                                                                |
| ------------------------ | ---------------------------------------------------------------------- |
| `POST /rpc`              | JSON management commands. `hello` and `server_info` are public probes. |
| `POST /release-artifact` | Streamed deploy artifacts signed over app, version, size, and digest.  |
| `POST /logs`             | Raw app log bytes with offset headers.                                 |

All non-probe requests require an enrolled SSH key signature. The signed request includes a key fingerprint, timestamp, nonce, and signature over the command body or endpoint metadata. Timestamps outside the auth window and replayed nonces are rejected.

`tako servers add` verifies the host is reachable over Tailscale, verifies SSH recovery access as `tako@host`, enrolls the SSH key used for that recovery connection, checks the server identity, verifies signed HTTP access, and records target metadata before writing global `config.toml`.

App/runtime commands such as deploy, status, logs, scale, releases, backups, delete, and secret sync use signed HTTP management. SSH remains for setup, recovery, reload, upgrade, and uninstall flows.

## Routing

Routes live under `[envs.<env>]`:

```toml
[envs.production]
routes = ["app.example.com", "*.example.com/api/*"]
servers = ["prod-a"]
```

The proxy picks the most specific matching host and path. Static files are served from the deployed `public/` directory when possible; other requests go to an app instance. Unknown hosts return `404`.

The `/_tako/*` path space is reserved after a route match. Tako uses it for durable channels, public image optimization, and signed local storage routes.

## TLS

Tako manages certificates automatically:

- Exact public hostnames use Let’s Encrypt HTTP-01 by default.
- Wildcard hostnames use Let’s Encrypt DNS-01 after `tako credentials set ssl.cloudflare --env <env>`.
- Environments with `ssl = "cloudflare"` use Cloudflare Origin CA after the same credential is set.
- Local and private hostnames such as `localhost`, `.test`, `.local`, `.invalid`, `.example`, and `.home.arpa` use self-signed certificates.

Cloudflare tokens for certificate operations are encrypted in `.tako/secrets.json` under the environment's provider credentials. Deploy fails early when selected providers need missing or expired credentials, verifies required tokens with Cloudflare before build/upload, and warns when credentials expire within 30 days. For Let’s Encrypt wildcard routes, the preflight also checks that the token can read the matching Cloudflare zone.

Cloudflare DNS-01 does not require Cloudflare proxy mode. Cloudflare SSL uses Origin CA certificates for Cloudflare-proxied traffic.

## Source IPs

`source_ip` is selected per app environment:

```toml
[envs.production]
route = "app.example.com"
source_ip = "direct"
```

Generated configs omit `source_ip`, which behaves like `auto`: use `CF-Connecting-IP` only for Cloudflare peers, then configured trusted proxy headers for trusted peers, then the direct TCP peer IP.

Use `cloudflare-proxy` when traffic must arrive through Cloudflare. Use `trusted-proxy` for a configured front proxy. Use `direct` to ignore proxy headers.

Cloudflare ranges are bundled, refreshed daily while needed, and cached on disk as a last-known-good fallback.

## App Processes

App instances bind to `127.0.0.1` on an OS-assigned port. The SDK writes that bound port to fd 4. `tako-server` only routes to the instance after that readiness signal succeeds.

Health checks hit the SDK-provided status endpoint:

```http
GET /status
Host: <app>.tako
X-Tako-Internal-Token: <token>
```

Ongoing health uses active HTTP probes, process exit detection, and replacement after repeated failures.

## Scale-To-Zero

New deployments start with one warm instance per server. Change the desired count with:

```bash
tako scale 0 --env production
tako scale 2 --env production
```

Desired instances are runtime state on each server and survive deploys, rollbacks, and server restarts. `0` means scale-to-zero: idle instances can stop after `idle_timeout`, and the next request waits for startup readiness. Startup timeouts return generic `504` responses; startup setup failures return generic `502` responses. Details are recorded in app logs.

## Secrets And Storage

Project secrets are encrypted in `.tako/secrets.json`. Each environment has a key id, encrypted app secrets, optional encrypted backup keys, optional encrypted storage credentials, and optional encrypted provider credentials. Expiry metadata is plaintext so deploy can fail early on expired credentials and warn on credentials expiring within 30 days.

Secrets and storage bindings are stored encrypted in server SQLite. Fresh HTTP instances and workflow workers receive them through fd 3 at spawn time, not through inherited process environment variables.

Storage bindings are declared in `tako.toml` and exposed to JavaScript apps as `tako.storages.<name>`. S3-compatible credentials are encrypted in `.tako/secrets.json`. The built-in `local` resource has no user credentials and serves signed app-local URLs under `/_tako/storages/<binding>/<key>`.

Backups reuse private S3-compatible storage resources but are not SDK bindings unless also declared in `[envs.<env>].storages`. The server backs up `data/app/` and durable workflow state under `data/tako/` after successful deploys and roughly every 24 hours. Transient channel replay storage is excluded and starts empty after restore. Archives are encrypted before being stored under `_tako/backups/{app}/{env}/{server}/` with 30-day retention by default.

## Workflows And Channels

Workflows are durable runs owned by `tako-server`. SDKs talk to a shared internal Unix socket for enqueue, signal, claim, heartbeat, step save, completion, cancellation, deferral, and channel publish commands. Every internal command carries the deployed app id.

Workflow workers can be always-on or scale-to-zero. With the default `workers = 0`, the server starts a worker on enqueue or cron tick, lets it process due work, and stops it after an idle window.

Durable channels are public app routes under `/_tako/channels/<name>`. Definitions live in app code; every publish is stored before delivery. Production uses a bounded SQLite replay log scoped to the deployed app id (`{name}/{env}`); local dev keeps one in-memory replay store per registered app. WS/SSE clients use the log to resume across short disconnects. Browser clients can pass `authorization: token` for header-auth channels, or omit it for public and cookie-auth channels. The default replay window is 10 minutes, and apps keep permanent history in their own database when they need one.

## Local Development

`tako dev` runs the same app model locally: HTTPS, real hostnames, fd-4 readiness, fd-3 bootstrap, local data dirs, workflow workers, channels, storage bindings, and public image routes.

Development routes default to `https://{app}.test/` on macOS and `https://{app}.test:47831/` on non-macOS unless `[envs.development]` defines routes. Managed `.test` and `.tako.test` routes are served by the local DNS/proxy setup. External routes can be added without replacing the default managed route.
