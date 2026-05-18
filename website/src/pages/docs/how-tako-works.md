---
layout: ../../layouts/DocsLayout.astro
title: "How Tako works: rolling deploys, TLS, health checks, and scale to zero - Tako Docs"
heading: "How Tako Works"
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako is a local CLI plus a self-hosted server runtime. The CLI owns project configuration, local development, builds, deploy orchestration, secrets, and server management. `tako-server` runs on your hosts and owns routing, TLS, app processes, workflow workers, channels, images, health checks, scale-to-zero, and rolling updates.

The protocol is v0. Runtime behavior lives in runtime plugins. Presets stay small: they provide framework entrypoints, asset roots, and dev commands.

## Main Pieces

| Piece               | Role                                                                                                                         |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `tako`              | Local CLI for init, dev, deploy, servers, secrets, storage, logs, status, scaling, and generated files.                      |
| `tako-server`       | Remote runtime with the proxy, TLS, app supervisor, state store, workflow manager, and management API.                       |
| `tako.sh`           | JavaScript/TypeScript SDK for the fetch runtime, status endpoint, channels, workflows, storage, images, and type generation. |
| `tako.sh` Go module | Go SDK for `net/http` handlers, readiness, health checks, secrets, channels, and workflow RPCs.                              |
| Runtime plugins     | Built-in runtime definitions for Bun, Node, and Go.                                                                          |
| Presets             | Framework defaults such as Vite, TanStack Start, and Next.js.                                                                |

## Project Identity

Each deployed app environment is identified on the server as:

```text
{app}/{env}
```

The app name comes from top-level `name` in `tako.toml`. If omitted, Tako derives it from the selected config file's parent directory. Setting `name` explicitly keeps the server-side identity stable if the directory is renamed.

The same app can deploy multiple environments to the same physical server because each environment gets its own identity and filesystem path.

## Deploy Flow

`tako deploy` targets `production` unless `--env` is provided. The target environment must exist in `tako.toml`, must define `route` or `routes`, and cannot be `development`.

The high-level flow is:

1. Validate config, routes, target servers, secrets, storage credentials, DNS credentials, and server target metadata.
2. Resolve runtime, preset, package manager, entrypoint, asset roots, and build commands.
3. Build locally in a temporary workspace.
4. Package the artifact while excluding `.git/`, `.tako/`, `.env*`, `node_modules/`, configured exclusions, and `.gitignore` matches.
5. Upload the artifact to each target server over signed HTTP; the server verifies size and digest before extracting it.
6. Ask each server to prepare the release by downloading the runtime if needed and installing production dependencies.
7. Run the optional release command once on the leader server.
8. Deploy route, source-IP, secret, storage, and DNS bindings through the signed management path.
9. Start the new instance set, wait for health, then drain old instances.

Servers receive prebuilt artifacts. They do not build application code during deploy.

## Remote Management

Normal server installs require Tailscale for remote management. The installer binds the management HTTP endpoint to the server's Tailscale address on port `9844`.

The HTTP management API uses the same typed `Command -> Response` protocol as the Unix management socket. `hello` and `server_info` are public probes. All other RPCs require an enrolled SSH key signature, a fresh timestamp, and a non-replayed nonce. Deploy artifacts and app logs use separate byte-body endpoints signed over request metadata.

`tako servers add` verifies Tailscale reachability, SSH recovery access as `tako@host`, management identity, signed RPC access, and server target metadata before it writes the server to global `config.toml`.

Normal app/runtime commands such as deploy, status, logs, scale, releases, delete, and secret sync use signed HTTP management. SSH remains for setup, recovery, reload, upgrade, and uninstall flows.

## Routing

Routes live under `[envs.<env>]`:

```toml
[envs.production]
routes = ["app.example.com", "*.example.com/api/*"]
servers = ["prod-a"]
```

The proxy selects the most specific matching host and path. Static files are served directly from the deployed `public/` directory when possible, then unmatched paths are proxied to the app. Unknown hosts return `404`.

The `/_tako/*` path space is reserved after a route match. Tako uses it for channels, public image optimization, and signed local storage routes.

## TLS

Tako manages certificates automatically:

- Exact public hostnames use HTTP-01 challenges.
- Wildcard hostnames use Cloudflare DNS-01 after the app environment is configured with `tako dns configure --env <env>`.
- Local/private hostnames such as `localhost`, `.test`, `.local`, `.invalid`, `.example`, and `.home.arpa` use self-signed certificates.

Cloudflare DNS tokens are encrypted in `.tako/secrets.json` under the selected environment's `dns` object. They are not written to `tako.toml`. Deploy fails early when wildcard routes need missing or expired DNS credentials, and warns when credentials expire within 30 days.

## Source IPs

`source_ip` is selected per app environment:

```toml
[envs.production]
route = "app.example.com"
source_ip = "cloudflare-proxy"
```

Generated configs omit `source_ip`. Omitted, or `"auto"`, uses `CF-Connecting-IP` when the peer IP belongs to Cloudflare, then uses explicitly configured trusted proxy headers when the peer is trusted, then falls back to the direct peer IP.

Use `"cloudflare-proxy"` when all public traffic must arrive through Cloudflare. Non-Cloudflare requests, or Cloudflare requests without a valid `CF-Connecting-IP`, are rejected with `403 Forbidden`.

Use `"trusted-proxy"` for nginx, HAProxy, Caddy, Traefik, or another front proxy. Requests must come from loopback or a CIDR in server `trusted_proxy.trusted_cidrs`, and must include a valid `X-Forwarded-For` or `Forwarded` client IP unless the server config sets `trusted_proxy.client_ip_headers`.

Use `"direct"` to ignore proxy headers and use the TCP peer IP.

Cloudflare IP ranges are held in memory, seeded from bundled ranges and a last-known-good disk cache, and refreshed every 24 hours while any active route uses `auto` or `cloudflare-proxy`.

## App Processes

App processes bind to `127.0.0.1` with `PORT=0`. The SDK opens an OS-assigned loopback port and writes the bound port to fd 4. `tako-server` only routes traffic after that readiness signal succeeds.

Health checks hit:

```http
GET /status
Host: <app>.tako
X-Tako-Internal-Token: <token>
```

The SDK wrappers implement the internal status endpoint and token check automatically. Ongoing health uses active HTTP probes, process exit detection, and replacement after repeated failures.

## Scale-To-Zero

New deployments start with one warm instance per server. Use `tako scale` to change the desired count:

```bash
tako scale 0 --env production
tako scale 2 --env production
```

Desired instances persist across server restarts, deploys, and rollbacks.

`0` means scale-to-zero. The app can stop after `idle_timeout` seconds and wake on the next request. The request waits for startup readiness up to the startup timeout; if no instance becomes healthy in time, the proxy returns a generic `504` and details go to app logs.

## Secrets And Storage

Project secrets are encrypted in `.tako/secrets.json`. Each environment has a key id, encrypted app secrets, optional encrypted storage credentials, and optional encrypted DNS credentials. Expiry metadata is plaintext so deploy can fail early for expired credentials and warn when credentials expire within 30 days.

Secrets and storage bindings are stored encrypted in server SQLite and delivered to app and worker processes through fd 3 at spawn time. They are not inherited through process environment variables.

Storage bindings are declared in `tako.toml` and exposed to JavaScript apps as `tako.storages.<name>`. S3-compatible credentials are encrypted in `.tako/secrets.json`; the built-in `local` resource has no user-provided credentials and serves signed app-local URLs under `/_tako/storages/<binding>/<key>`.

## Workflows And Channels

Workflows are durable runs owned by `tako-server`. SDKs talk to a shared internal Unix socket for enqueue, signal, claim, heartbeat, step save, completion, cancellation, deferral, and channel publish commands. Every internal command carries the deployed app id.

Workflow workers can be always-on or scale-to-zero. With the default `workers = 0`, the server starts a worker on enqueue or cron tick, lets it process due work, and stops it after an idle window.

Channels are public app routes under `/_tako/channels/<name>`. Definitions live in app code, while durable channel storage and server-side publish go through Tako's runtime.

## Local Development

`tako dev` runs the same app model locally: HTTPS, real hostnames, fd-4 readiness, fd-3 bootstrap, local data dirs, workflow workers, channels, storage bindings, and public image routes.

Development routes default to `https://{app}.test/` on macOS and `https://{app}.test:47831/` on non-macOS unless `[envs.development]` defines routes. Managed `.test` and `.tako.test` routes are served by the local DNS/proxy setup. External routes, such as Cloudflare Tunnel hostnames, can be added without replacing the default managed route.
