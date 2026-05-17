---
layout: ../../layouts/DocsLayout.astro
title: "Local development with Tako dev: HTTPS, domains, and hot reload - Tako Docs"
heading: Development
current: development
description: "Learn how tako dev provides trusted HTTPS, custom .test domains, hot reload, variants, and a persistent local background daemon."
---

# Local Development With Tako

`tako dev` runs your app behind trusted local HTTPS and a real hostname:

```bash
tako dev
```

Default URL:

```text
https://{app}.test/
```

The app name comes from `name` in `tako.toml`, or from the selected config file's parent directory when `name` is omitted.

## What Starts

`tako dev` is a client for a persistent local daemon:

1. It prepares DNS, TLS, and proxy prerequisites.
2. It starts `tako-dev-server` when needed.
3. It registers the selected config file with the daemon.
4. It starts the app process and waits for fd-4 readiness.
5. It streams logs and status into your terminal.

Running `tako dev` again for the same config attaches to the existing session when the app is running or idle.

When running from a source checkout, Tako prefers repo-local `target/debug` or `target/release` dev binaries. Installed CLIs use bundled or PATH binaries.

## Routes

Default managed route:

```text
https://{app}.test/
```

Configure development routes in `tako.toml`:

```toml
[envs.development]
routes = [
  "app.test",
  "api.app.test/api/*",
  "dev.example.com"
]
```

Managed local DNS applies to `.test` and `.tako.test` routes. External routes are still routable by the local proxy, but Tako does not resolve them or advertise them in LAN mode.

If explicit managed `.test` or `.tako.test` routes are configured, they replace the default `{app}.test` route. If only external routes are configured, Tako keeps the default managed route and adds the external routes.

Unknown managed local DNS hosts return a helpful `421` that lists registered dev routes. Unknown `.local` LAN hosts and unknown external hosts return a generic `421`.

## Variants

```bash
tako dev --variant preview
tako dev --var preview
```

Variants create a DNS-specific session such as:

```text
https://{app}-preview.test/
```

Use variants when you need two local copies of the same app shape reachable at different hostnames.

## Platform Setup

### macOS

Tako uses:

- a dedicated loopback alias: `127.77.0.1`
- `/etc/resolver/test` and `/etc/resolver/tako.test`
- a local DNS listener on `127.0.0.1:53535`
- a launchd-managed dev proxy for `127.77.0.1:80` and `127.77.0.1:443`
- a local root CA trusted in the system keychain

The dev proxy forwards:

```text
127.77.0.1:443 -> 127.0.0.1:47831
127.77.0.1:80  -> 127.0.0.1:47830
```

Tako installs and repairs the proxy when needed. It explains the change before prompting for sudo.

### Linux

Tako uses:

- the same loopback alias: `127.77.0.1`
- iptables redirects for `443 -> 47831`, `80 -> 47830`, and `53 -> 53535`
- systemd-resolved routes for `~test` and `~tako.test`
- a local root CA trusted by the system store

On NixOS, Tako prints a `configuration.nix` snippet instead of applying imperative setup.

## Local CA

The first dev run creates a root CA under `{TAKO_HOME}/ca/`:

```text
{TAKO_HOME}/ca/ca.crt
{TAKO_HOME}/ca/ca.key
```

The private key is mode `0600`. Leaf certificates are generated on demand for app domains. The CA is scoped to `{TAKO_HOME}` so separate data directories do not share trust material.

## App Lifecycle

Dev app statuses:

| Status    | Meaning                                                |
| --------- | ------------------------------------------------------ |
| `running` | Process is active and serving.                         |
| `idle`    | Process is stopped, routes remain for wake-on-request. |
| `stopped` | App is unregistered, routes are removed.               |

The app starts immediately when `tako dev` starts. If no CLI client remains attached for 30 minutes, it can go idle. The next request wakes it and waits for readiness.

Pressing `Ctrl-C` unregisters the app, removes routes, and kills the process. Pressing `b` backgrounds it, leaving the daemon to keep routes active.

## Keyboard Shortcuts

In an interactive terminal:

| Key      | Action                               |
| -------- | ------------------------------------ |
| `r`      | Restart the app process.             |
| `l`      | Toggle LAN mode.                     |
| `b`      | Background the app and exit the CLI. |
| `Ctrl-C` | Stop and unregister the app.         |

When stdout is not a terminal, `tako dev` falls back to plain output with no color or raw keyboard mode.

## LAN Mode

Press `l` to expose registered dev routes on `.local` aliases.

Example:

```text
app.test       -> app.local
app.test/api/* -> app.local/api/*
```

Concrete hostnames are advertised with mDNS. Wildcard routes cannot be advertised because mDNS has no wildcard records; they still work for clients that resolve the wildcard host through another DNS path. Tako surfaces a warning for wildcard LAN routes and suggests adding an explicit subdomain route.

## Logs

Interactive `tako dev` prints a header, then streams logs and lifecycle lines in the same terminal. It does not use an alternate screen, so normal scrollback, search, copying, and clickable links still work.

Log lines look like:

```text
hh:mm:ss INFO [app] server ready
```

Common scopes:

- `tako`: local dev daemon
- `app`: app process
- `worker`: workflow worker process

Tako infers levels from app output that starts with tokens such as `DEBUG`, `INFO`, `WARN`, `ERROR`, and `FATAL`.

Dev logs are persisted at:

```text
{TAKO_HOME}/dev/logs/{app}-{hash}.jsonl
```

Attached clients replay the existing file and then follow new records.

## Hot Reload And Restarts

Source hot reload is runtime-driven, for example by Vite, Bun, or your custom dev command.

Tako watches:

- `tako.toml`
- `.tako/secrets.json`
- `<app_root>/channels/`
- `<app_root>/workflows/`
- locations that may contain `tako.d.ts`: `app/`, `src/`, and project root

Tako restarts the app when effective dev environment variables, secrets, storage bindings, channel definitions, or workflow definitions change. Route changes under `[envs.development]` update routing without restarting the app.

## App Root And Generated Files

For JS apps, `app_root` controls where channels and workflows live:

```toml
app_root = "src"
```

Default: `src`.

Use `app_root = "."` when `channels/`, `workflows/`, or `tako.d.ts` live next to `tako.toml`.

`tako dev`, `tako deploy`, `tako generate`, and secret changes regenerate `tako.d.ts` as needed. The generated file augments `tako.sh` with project environment names, secret keys, storage binding names, channel metadata, workflow metadata, and runtime env globals.

## Environment Variables

Dev loads:

1. `[vars]`
2. `[vars.development]`
3. Tako runtime variables

Common values:

| Name            | Value in dev                            |
| --------------- | --------------------------------------- |
| `ENV`           | `development`                           |
| `PORT`          | `0`; the SDK binds an OS-assigned port  |
| `HOST`          | `127.0.0.1`                             |
| `TAKO_BUILD`    | `dev`                                   |
| `TAKO_DATA_DIR` | Persistent app-owned dev data directory |
| `TAKO_APP_ROOT` | JS app root, default `src`              |
| `NODE_ENV`      | `development` for JS runtimes           |
| `BUN_ENV`       | `development` for Bun                   |

`ENV` is reserved and always derived by Tako.

## Secrets In Dev

Secrets are read from `.tako/secrets.json` and exposed through the SDK:

```ts
import { tako } from "tako.sh";

const db = tako.secrets.DATABASE_URL;
```

Secret entries can carry expiry metadata for deployment checks; deploy fails on expired secrets and warns when they expire within 30 days. Dev exposes configured secret values so local work keeps using the same SDK shape as production.

The fd-3 bootstrap envelope is present in dev even when no secrets exist. It carries internal auth, the secrets object, and storage bindings.

## Storage In Dev

Storage bindings are read from `[envs.development].storages` in `tako.toml` and exposed through the SDK:

```ts
import { tako } from "tako.sh";

const uploadUrl = await tako.storages.uploads.createUploadUrl("avatars/u_123.png", {
  contentType: "image/png",
});
```

Development uses the `development` storage bindings when present. A development binding can reference an undeclared resource; Tako treats it as local storage with a Tako-chosen location under the app data directory. If no development bindings exist, `tako generate` falls back to the union of storage names from other environments for type generation.

## Images In Dev

Public optimized images are served at:

```text
/_tako/image?src=/assets/hero.jpg&w=1200
```

Local image sources are allowed by default. Remote image sources must match `[images].remote_patterns` in `tako.toml`; protocol-less remote patterns allow both `http` and `https`. Use `imageUrl` for one optimized URL or `imageSrcSet` for plain `<img>` responsive sources.

## Channels In Dev

Channel files live under:

```text
<app_root>/channels/
```

Public channel routes use:

```text
/_tako/channels/<name>
```

SSE and WebSocket behavior matches production. Server-side channel publishes go through the internal socket, not back through the public HTTPS proxy.

## Workflows In Dev

Workflow files live under:

```text
<app_root>/workflows/
```

Dev uses the same architecture as production: `tako-dev-server` owns the runs database and internal socket, while worker subprocesses execute user workflow code.

Workers are scale-to-zero with a short idle timeout. They start on enqueue or cron tick, exit when idle, and are spawned fresh on the next wake so code edits apply without restarting `tako dev`.

If a worker exits non-zero before claiming a run, enqueue fails loudly for a short window with the worker error. Clean idle exits do not mark the worker unhealthy.

## Stopping And Listing Apps

```bash
tako dev list
tako dev stop
tako dev stop my-app
tako dev stop --all
```

`tako dev list` has alias `tako dev ls`.

Without a name, `tako dev stop` stops the app for the selected config file.

## Diagnostics

Use:

```bash
tako doctor
```

Doctor reports local daemon, DNS, proxy, loopback, and platform setup. If the daemon is not running, it reports that state and exits successfully.
