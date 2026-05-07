---
layout: ../../layouts/DocsLayout.astro
title: "How Tako works: rolling deploys, TLS, health checks, and scale to zero - Tako Docs"
heading: How Tako Works
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako is a development and deployment platform made of three pieces:

- `tako`: the local CLI
- `tako-server`: the process running on each deployment host
- `tako.sh`: the app SDK for JavaScript/TypeScript and Go

The CLI builds and ships your app. The server terminates TLS, routes traffic, manages processes, runs health checks, stores runtime state, and performs rolling updates. The SDK adapts your app to Tako's runtime protocol.

## Management Path

Management actions start on your machine:

```bash
tako deploy --env production
tako scale 2 --env production
tako secrets sync
tako releases rollback abc1234 --env production
```

The CLI connects to each target server over SSH and sends JSON commands to the server's Unix management socket:

```text
/var/run/tako/tako.sock
```

During a graceful server reload, the stable socket path is a symlink to a PID-specific socket. The new server process swaps the symlink atomically when it is ready, so management clients connect to the active process.

## Traffic Path

Production traffic enters `tako-server` through Pingora:

1. TLS is selected by SNI.
2. HTTP is redirected to HTTPS except ACME challenge paths.
3. The request host and path are matched against deployed app routes.
4. Static assets are served directly from the app's deployed `public/` directory when possible.
5. Dynamic requests go to an app instance over loopback TCP.

Apps bind to `127.0.0.1` on an OS-assigned port. The SDK writes the actual port to fd 4 when the app is ready. Tako then routes traffic to that private endpoint.

## App Identity

Remote apps are identified as:

```text
{app}/{env}
```

The app name comes from `name` in `tako.toml`, or from the selected config file's parent directory when `name` is omitted.

This lets one server host the same app in multiple environments:

```text
dashboard/production
dashboard/staging
```

## Deploy Flow

`tako deploy` targets one environment, defaulting to `production`.

At a high level it:

1. validates `tako.toml`, secrets, routes, and server target metadata
2. resolves runtime, package manager, preset, entrypoint, and build stages
3. copies the source bundle into `.tako/build`
4. runs build commands in the build directory
5. merges static assets into `public/`
6. writes `app.json`
7. creates a target-specific artifact
8. uploads and extracts it on each server
9. runs production dependency install on each server
10. runs the release command on the leader server when configured
11. performs rolling update on each server

Servers receive prebuilt artifacts. They do not run app build steps.

## Rolling Updates

Rolling update happens per server:

1. start one new instance
2. wait for health checks
3. add it to the load balancer
4. drain and stop one old instance
5. repeat until the release is current
6. update the `current` symlink

If startup or health checks fail, Tako kills new instances and keeps the old release serving.

Deploys are serialized per deployed app id. A second deploy for the same app and environment on the same server fails immediately with a retry message.

## Health Checks

Tako actively probes each instance:

```http
GET /status
Host: tako.internal
X-Tako-Internal-Token: <instance-token>
```

The SDK implements this endpoint. Probes use the private loopback endpoint, not the public proxy.

Health checks run every second during steady state and every 100ms while an instance is starting. A single failure after a successful probe marks the instance dead and triggers replacement.

## Scaling

Desired instance count is server-side runtime state, not `tako.toml` config.

New deployments start with one desired instance per server. Change it with:

```bash
tako scale 2 --env production
tako scale 0 --env production
tako scale 1 --server la
```

Desired count persists across deploys, rollbacks, and server restarts.

When desired count is `0`, the app scales to zero after its idle timeout. The next request triggers a cold start. If startup succeeds, the queued request continues. If startup fails or times out in production, the proxy returns `502 Bad Gateway`, `503 Service Unavailable`, or `504 Gateway Timeout` with a generic body; diagnostics include captured startup stdout/stderr in logs when available.

## Routing

Routes are configured per environment:

```toml
[envs.production]
routes = ["api.example.com", "example.com/app/*", "*.example.com/admin/*"]
```

Tako supports exact hosts, wildcard hosts, and path-prefixed routes. The most specific matching route wins. Unknown hosts return `404`.

For static assets, Tako checks the deployed `public/` directory. Path-prefixed routes also try prefix-stripped asset lookup, so `/app/assets/main.js` can serve `/assets/main.js`.

## TLS

Tako uses SNI to choose certificates:

1. exact certificate match
2. wildcard fallback
3. default self-signed fallback so HTTPS can complete

Public hostnames use ACME. Private or local hostnames such as `localhost`, `.local`, `.test`, `.invalid`, `.example`, and `.home.arpa` use self-signed certificates.

Wildcard routes require DNS-01 support. Configure it with:

```bash
tako servers setup-wildcard --env production
```

## Secrets

Local secret source of truth is `.tako/secrets.json`. Values are encrypted per environment with AES-256-GCM. The first secret set for an environment creates a random environment key, using the key id stored in `.tako/secrets.json`. By default keys are stored under Tako's data directory as `keys/{key_id}`. On macOS, interactive key creation and import offer iCloud Keychain storage through the signed `Tako.app` CLI installed by the macOS installer. If the signed app entitlement is unavailable, Tako fails before writing a local key file or updating `.tako/secrets.json`. Exported keys are single base64url strings that can be imported on another machine; on macOS, export requires user authentication before copying the key. Passphrase import derives the environment key from a passphrase and the environment key id.

Deploy sends secrets only when the server-side hash differs. Long-running app and worker processes receive secrets through fd 3 at spawn time, not through env vars. Release commands are one-shot and receive secrets as env vars.

## Runtime Data

Each deployed app gets persistent data directories under:

```text
/opt/tako/apps/{app}/{env}/data/
```

The app-owned path is exposed as `TAKO_DATA_DIR`.

## Workflows and Channels

Workflow queues and channel storage are owned by `tako-server`. SDKs communicate with the server through a per-app internal Unix socket using `TAKO_INTERNAL_SOCKET` and `TAKO_APP_NAME`.

Workflows run in separate worker processes from HTTP instances. Workers can be always-on or scale-to-zero. Runs, steps, schedules, and event waiters are stored in SQLite under the app data directory.

Channels are durable pub-sub streams available at:

```text
GET /channels/<name>
```

For JavaScript/TypeScript apps, `<name>` is the explicit `name` property passed
to `defineChannel({ name: "<name>", ... })`. SSE and WebSocket transports are supported,
with bounded replay for reconnects. Browser clients retry indefinitely across
network loss, laptop sleep, server restarts, and stream rotation, then resume
from the last received message id while it remains inside the replay window.

## Local Development

`tako dev` talks to a persistent `tako-dev-server` daemon. The daemon owns local HTTPS, `.test` DNS routing, app process lifecycle, logs, wake-on-request, and workflow workers. Development routes can also include external hostnames when another tool points those hosts at the dev proxy. Unknown `.local` LAN hosts and unknown external hosts get a generic `Misdirected Request` 421 response without route details.

Local app URLs are based on the app name:

```text
https://dashboard.test/
```

On macOS, Tako uses a launchd-managed loopback proxy for portless `:80` and `:443` URLs. On Linux, it uses a loopback alias and redirect rules.

The local CA is generated once, stored under Tako's home directory, and trusted through the system trust store after a one-time privileged setup.
