---
layout: ../../layouts/DocsLayout.astro
title: "Local Development With Tako - Tako Docs"
heading: Development
current: development
description: "Learn how tako dev provides trusted HTTPS, custom .test domains, hot reload, variants, and a persistent local background daemon."
---

# Local Development With Tako

`tako dev` runs your app behind trusted local HTTPS and real hostnames:

```bash
tako dev
```

By default, the app is available at:

```text
https://<app>.test/
```

On non-macOS platforms without the portless proxy path, the registered HTTPS daemon port is `47831`. On macOS and supported Linux setup, Tako uses system routing so browser URLs stay portless.

## What Starts

`tako dev` is a client for a persistent local daemon. It:

- starts or reuses `tako-dev-server`
- prepares the local CA, DNS, loopback, and proxy/redirect setup
- loads `tako.toml` and `.tako/secrets.json`
- generates runtime-specific files when the selected SDK needs them
- injects secrets and storage bindings through fd 3
- waits for fd-4 readiness before routes become active
- registers HTTPS routes with the daemon
- attaches logs and interactive controls

The daemon keeps running after the app backgrounds, and it can host multiple dev apps.

If `container = "Dockerfile"` is set for deploys, `tako dev` still runs the configured dev command, preset dev command, or native runtime default. It does not build or run the container file locally. If deploy uses `start` for a built native artifact, set `dev` separately for the local development command.

The local dev proxy does not apply Tako's deployed-response Brotli/gzip compression, so browser debugging shows the app response body directly.

## Interactive Controls

| Key      | Action                                        |
| -------- | --------------------------------------------- |
| `r`      | Restart the app.                              |
| `l`      | Toggle LAN mode for managed local routes.     |
| `t`      | Toggle a temporary public tunnel URL.         |
| `b`      | Background the app and exit the attached CLI. |
| `Ctrl+c` | Stop the app and exit.                        |

The status panel always shows `routes`, `lan`, and `tunnel`. `lan` shows an enable hint while off, the active URL while on, or a retry hint after failure. `tunnel` also shows its async starting state while connecting and the disable hint below the active URL.

Manage running apps:

```bash
tako dev list
tako dev stop
tako dev stop my-app
tako dev stop --all
```

`tako dev list` also has alias `tako dev ls`.

Start with a public tunnel already enabled:

```bash
tako dev --tunnel
```

## Variants

Use variants for isolated dev sessions:

```bash
tako dev --variant preview
tako dev --var preview
```

Variants get isolated local runtime state while sharing project config. A variant changes the managed route slug, for example `my-app-preview.test`.

## Local HTTPS

Tako creates a local root CA once per `{TAKO_HOME}`:

```text
{TAKO_HOME}/ca/ca.crt
{TAKO_HOME}/ca/ca.key
```

It installs the root into the system trust store with a sudo prompt when needed. Leaf certificates are generated for app domains by SNI. The public CA certificate can be used by tools that need explicit trust:

```bash
export NODE_EXTRA_CA_CERTS="{TAKO_HOME}/ca/ca.crt"
```

## macOS Networking

On macOS, Tako installs and repairs a launchd-managed loopback proxy and resolver setup when needed.

DNS:

```text
/etc/resolver/test
/etc/resolver/tako.test
```

Proxying:

```text
127.77.0.1:443 -> 127.0.0.1:47831
127.77.0.1:80  -> 127.0.0.1:47830
```

The launchd proxy is socket-activated and may exit after a long idle window. Launchd reactivates it on the next request.

Because the local proxy connects from loopback, its forwarded HTTPS metadata is trusted and local HTTP redirects do not loop.

## Linux Networking

On Linux, Tako uses:

- loopback alias `127.77.0.1`
- iptables redirects for `443 -> 47831`, `80 -> 47830`, and `53 -> 53535`
- systemd-resolved routes for `~test` and `~tako.test`
- a local CA trusted by the system store

On NixOS, Tako prints a `configuration.nix` snippet instead of making imperative changes.

## Routes

If `[envs.development]` is missing or has no routes, Tako registers:

```text
<app>.test
```

Configured managed local routes replace that default:

```toml
[envs.development]
routes = ["app.test", "api.app.test/*"]
```

External development routes are additive when no managed `.test` or `.tako.test` route is configured:

```toml
[envs.development]
routes = ["my-tunnel.example.com"]
```

Tako manages DNS and LAN aliases only for `.test` and `.tako.test`. External hostnames are routed by the local proxy but are not advertised with mDNS and are not resolved by Tako DNS.

Unknown managed `.test` or `.tako.test` hosts return a helpful 421 response listing registered routes. Unknown `.local` LAN hosts and external hosts return a generic 421.

## LAN Mode

Press `l` during `tako dev` to expose managed local routes on `.local` aliases. For example:

```text
app.test -> app.local
```

Wildcard dev routes participate in proxy routing but cannot be advertised with mDNS.

## Tunnel Mode

Press `t` during `tako dev` to create a temporary public HTTPS URL for the current app. Tunnel hostnames use:

```text
<nonce>.tako.website
```

The nonce is generated by the tunnel service, so tunnel mode does not require login or namespace setup. Tunnels expire after the service TTL, currently 30 minutes, and are also turned off when the app stops or the local tunnel connection closes.

`tako dev list` shows the current tunnel URL for apps with tunnel mode enabled.

## Environment Variables

Development loads:

- `[vars]`
- `[vars.development]`
- runtime defaults such as `NODE_ENV=development` or `BUN_ENV=development`
- `ENV=development`
- `TAKO_BUILD=dev`
- `TAKO_DATA_DIR`
- `TAKO_APP_ROOT` for JS apps
- HTTP bind vars such as `PORT` and `HOST`

Non-string TOML scalar vars are stringified.

## Secrets

Development secrets come from `.tako/secrets.json`:

```bash
tako secrets set DATABASE_URL --env development
```

The fd-3 bootstrap envelope is present even when no secrets exist. It carries the internal auth token, secrets object, and storage bindings. App code reads secrets from the SDK:

```ts
import { tako } from "tako.sh";

const databaseUrl = tako.secrets.DATABASE_URL;
```

Changing secrets restarts the app so fresh processes receive the new fd-3 data.

## Storage

Development storage bindings come from `[envs.development].storages`:

```toml
[envs.development]
storages = { uploads = "local" }
```

Development can use the built-in `local` resource or any undeclared resource name; undeclared development resources default to local storage under the app data directory.

SDK usage:

```ts
import { tako } from "tako.sh";

const uploadUrl = await tako.storages.uploads.createUploadUrl("avatars/u_123.png", {
  contentType: "image/png",
});
```

Local storage URLs are app-relative signed routes under `/_tako/storages/<binding>/<key>`.

Backup storage is not exposed through the SDK unless the same resource is also listed under `[envs.<env>].storages`.

## Generated Types

For JS/TS projects, `tako dev`, `tako deploy`, `tako generate`, and secret changes refresh `tako.d.ts` as needed. The declaration file augments `tako.sh` with:

- environment names
- secret keys
- storage binding names
- channel metadata
- workflow metadata
- user-defined env var names for `process.env` and `import.meta.env`

`app_root` controls where channels, workflows, and preferred declarations live:

```toml
app_root = "src"
```

Use `app_root = "."` when `channels/`, `workflows/`, or `tako.d.ts` live next to `tako.toml`.

## Channels

Channel files live in:

```text
<app_root>/channels/
```

Example:

```ts
import { defineChannel } from "tako.sh";

export default defineChannel("chat", {
  auth: "public",
}).$messageTypes<{
  message: { text: string };
}>();
```

Channels are served at:

```text
/_tako/channels/<name>
```

`tako generate` scaffolds demo channel files for empty existing channel directories and adds missing default exports when definition files do not have one.

## Workflows

Workflow files live in:

```text
<app_root>/workflows/
```

Example:

```ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow<{ to: string }>("send-email", {
  async handler(payload, ctx) {
    await ctx.run("send", async () => {
      ctx.logger.info("sending", { to: payload.to });
    });
  },
});
```

Dev uses the same architecture as production: `tako-dev-server` owns the runs DB, dispatches runnable workflow work, and starts a worker subprocess on demand. Workers are scale-to-zero in dev with a short idle timeout, so code edits take effect on the next runnable enqueue, signal, or cron tick.

Go workflow handlers live in `cmd/worker/main.go`. When that file exists, `tako dev` runs the HTTP app with `go run .` and the worker with `go run ./cmd/worker`.

Broken workflow imports fail fast. If the worker exits non-zero before claiming any run, enqueue returns a worker-unhealthy error instead of silently queueing work.

## Images

Public optimized images are served at:

```text
/_tako/image
```

Use SDK helpers:

```ts
import { imageUrl, imageSrcSet } from "tako.sh";

const src = imageUrl("/hero.jpg", { width: 1200 });
const responsive = imageSrcSet("/hero.jpg", {
  width: 1200,
  layout: "constrained",
});
```

Local image sources are allowed by default. Remote image sources must match `[images].remote_patterns` in `tako.toml`. WebP is the default output format; AVIF is available when configured and requested.

## Watching And Restarts

Tako watches:

- `tako.toml`
- `.tako/secrets.json`
- `<app_root>/channels/`
- `<app_root>/workflows/`
- parent directories that can contain `tako.d.ts`

It restarts the app when effective vars, secrets, storage bindings, channel definitions, or workflow definitions change. It updates routes without restarting when `[envs.development].route(s)` changes.

Source hot reload is owned by your runtime or framework dev command. Tako does not watch arbitrary app source files for restart.

## Debugging Dev Setup

Run:

```bash
tako doctor
```

Doctor reports local dev setup, daemon state, platform proxy/DNS status, loopback configuration, CA trust, and repair hints. It exits successfully when the dev daemon is simply not running.
