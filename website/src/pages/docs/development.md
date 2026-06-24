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

Default URL:

```text
https://<app>.test/
```

The CLI is a client for a persistent `tako-dev-server` daemon. It starts the daemon when needed, registers the selected config file, streams logs, and attaches to existing sessions.

## What `tako dev` Does

- Ensures local TLS files and the local CA exist.
- Installs or repairs platform routing/proxy setup when needed.
- Registers the app config with the daemon.
- Generates project files such as `tako.d.ts` or Go secret accessors.
- Starts the app runtime and waits for fd-4 readiness from the HTTP process.
- Watches Tako config, secrets, channels, workflows, and generated-file locations.
- Restarts the app runtime when effective env, secrets, channels, or workflows change.
- Updates routes without restart when development routes change.

Tako does not watch arbitrary source files for restart. Framework dev servers and runtimes own hot reload.

## Routes

If `container = "Dockerfile"` is set for deploys, `tako dev` still runs the configured dev command, preset dev command, or native runtime default. It does not build or run the container file locally. If deploy uses `start` for a built native artifact, set `dev` separately for the local development command.

The local dev proxy does not apply Tako's deployed-response Brotli/gzip compression, so browser debugging shows the app response body directly.

Without development routes, Tako registers:

```text
https://<app>.test/
```

Configure local routes in `tako.toml`:

```toml
[envs.development]
routes = ["app.test", "api.app.test/api/*"]
```

Managed `.test` and `.tako.test` routes replace the default route. External development routes are additive unless you have configured at least one managed route; you must point external DNS or tunnels at the local proxy yourself.

Both `.test` and `.tako.test` are available. `.tako.test` is a fallback when another tool owns `.test`.

## Platform Networking

On macOS, Tako writes split-DNS resolver files for `.test` and `.tako.test`, uses a dedicated loopback alias (`127.77.0.1`), and installs a launchd socket-activated dev proxy:

```text
127.77.0.1:443 -> 127.0.0.1:47831
127.77.0.1:80  -> 127.0.0.1:47830
```

On Linux, Tako uses the same loopback alias with iptables redirect rules for HTTPS, HTTP, and DNS. On NixOS, it prints a `configuration.nix` snippet instead of applying imperative setup.

The HTTPS daemon itself listens on `127.0.0.1:47831`.

## Local CA

Tako creates one root CA per Tako home. The public cert is stored at `{TAKO_HOME}/ca/ca.crt`, and the private key is stored beside it with mode `0600`.

On first run, Tako asks for elevated access to install the CA into the system trust store. Once trusted, browser HTTPS warnings go away. The CA cert path can also be used as `NODE_EXTRA_CA_CERTS` for tools that need to trust local routes.

## App Lifecycle

The app starts when `tako dev` starts. If no CLI client is attached for 30 minutes, the daemon stops the process but keeps routes registered. The next HTTP request wakes the app and waits for readiness.

Statuses:

| Status    | Meaning                                                   |
| --------- | --------------------------------------------------------- |
| `running` | Process is live and serving.                              |
| `idle`    | Process is stopped, routes remain, next request wakes it. |
| `stopped` | App is unregistered and routes are removed.               |

`Ctrl-C` unregisters the app, removes routes, and stops the app runtime. Press `b` to background the app and leave it registered in the daemon.

## Interactive Controls

| Key      | Action                                |
| -------- | ------------------------------------- |
| `r`      | Restart the app runtime.              |
| `l`      | Toggle LAN `.local` aliases.          |
| `t`      | Toggle a temporary public tunnel URL. |
| `b`      | Background the app and exit the CLI.  |
| `Ctrl-C` | Stop and unregister the app.          |

LAN mode rewrites managed `.test` and `.tako.test` routes to `.local` aliases and advertises concrete hostnames with mDNS. Wildcard routes cannot be advertised with mDNS, so phones and tablets need an explicit concrete subdomain route.

Tunnel mode creates a temporary public HTTPS URL through the Tako tunnel service. URLs are stable for the same app and Tako Identity and stay enabled until you turn tunnel mode off or unregister the app. If the tunnel connection drops, Tako keeps the URL reserved and reconnects automatically.

## Variants

```bash
tako dev --variant staging
tako dev --var staging
```

Variants run a DNS variant of the app, such as `myapp-staging.test`.

## Logs

Interactive `tako dev` prints a header, then streams logs and lifecycle state directly to stdout. App output is grouped with scopes such as `app`, `worker`, and `tako`.

Dev logs are stored in a per-app/per-config JSONL stream at `{TAKO_HOME}/dev/logs/{app}-{hash}.jsonl`. New owning sessions truncate the stream. Attached sessions replay existing records and then follow new records.

When stdout is not a terminal, `tako dev` falls back to plain output with no raw mode.

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
<app>-<id>.tako.website
```

The id is derived from the app name and the local Tako Identity public key, so the same app gets the same URL when the same identity is available. On macOS, Tako tries iCloud Keychain for the identity and falls back to local storage when synced Keychain access is unavailable. Other platforms use local identity storage. The tunnel service issues a nonce and only creates the tunnel after the client signs it, so tunnel mode does not require login or namespace setup.

Starting a tunnel for the same app and identity replaces any previous active tunnel for that URL. Tunnels do not have a fixed session TTL. If the local tunnel connection is lost, `tako dev` keeps tunnel mode on, shows the URL as reconnecting, retries with bounded exponential backoff, and prints when reconnecting starts and when the tunnel reconnects. Tunnels turn off when you disable tunnel mode or unregister the app.

One Tako Identity can have up to five active tunnel URLs connected at the same time. Reconnecting or replacing the same app URL does not consume another slot. Starting a sixth active tunnel accepts the new tunnel and closes the oldest active tunnel for that identity; the closed client turns tunnel mode off and prints why it was closed.

When a tunnel URL is inactive or disconnected, browser requests get a Tako-styled error page. Clients that send `Accept: application/json` get JSON, and other clients get plain text.

`tako dev list` shows the current tunnel URL for apps with tunnel mode enabled.

## Environment Variables

Development processes receive:

| Name            | Value                                                     |
| --------------- | --------------------------------------------------------- |
| `ENV`           | `development`                                             |
| `NODE_ENV`      | `development` for JS runtimes                             |
| `BUN_ENV`       | `development` for Bun                                     |
| `PORT`          | `0`; SDK binds an OS-assigned port and reports it to Tako |
| `HOST`          | `127.0.0.1`                                               |
| `TAKO_BUILD`    | `dev`                                                     |
| `TAKO_DATA_DIR` | Persistent app-owned dev data dir                         |
| `TAKO_APP_ROOT` | JS app root, default `src`                                |

User variables come from `[vars]` plus `[vars.development]`.

The fd-3 bootstrap envelope is present even with no secrets or storages. It carries the internal auth token, `secrets`, and `storages`.

## One-Off Local Commands

Use `tako run` to run local scripts with the same project vars and SDK bootstrap shape:

```bash
tako run scripts/foo.ts
tako run --eval 'console.log(tako.env)'
tako run -- cargo run --bin migrate
```

`--env` defaults to `development`. Script files use the selected runtime's local rule, such as Bun/Node for JS/TS and `go run` for `.go` files. `--eval` runs inline source when the runtime supports it; JS runtimes support inline TypeScript. Use `-- {command...}` for exact commands.

The command runs from the app directory, sets `ENV`, `TAKO_BUILD=local`, `TAKO_DATA_DIR`, runtime defaults, and JS `TAKO_APP_ROOT`, then passes app secrets and storage bindings through `TAKO_BOOTSTRAP_DATA`. SDK-aware scripts use the same app SDK surfaces; JS/TS scripts can import `tako` and read `tako.secrets`.

Secrets are not raw process env vars.

## Secrets And Storage In Dev

Set development secrets:

```bash
tako secrets set DATABASE_URL --env development
```

Use them through the SDK:

```ts
import { tako } from "tako.sh";

const url = tako.secrets.DATABASE_URL;
```

Storage bindings from `[envs.development].storages` are delivered through `tako.storages`. In development, undeclared storage resource names default to local storage. Backup storage is not exposed to the SDK unless the same resource is also bound as app storage.

Use `tako.cache` for server-side caching of JSON-serializable values. `get<T>(key)` returns `T | undefined`, `put(key, value, { ttl })` stores a value with a TTL in milliseconds, and `delete(key)` removes one key. Cache entries use Tako-managed local SQLite and are not included in app data backups.

## Channels And Workflows

JS channels and workflows are discovered under `<app_root>/channels/` and `<app_root>/workflows/`. `tako dev` watches those directories and refreshes generated metadata when files change.

Channels are broadcast streams, not work queues. Multiple local clients subscribed to the same channel read the same app events, with independent cursors for reconnect replay. Dev channel replay stays in the dev daemon's in-memory store until the daemon restarts.

Workflows in dev use the same architecture as production: the dev daemon owns the runs database, internal socket, dispatcher, and scale-to-zero worker supervisor. The worker exits after a short idle window and restarts for the next runnable enqueue, signal, cron tick, or retry, so code edits take effect on fresh work.

Worker stdout/stderr is tee'd into the same log stream with `scope: "worker"`.

## Vite And Framework Dev Servers

Direct Vite dev commands must use the `tako.sh/vite` plugin for fd-4 readiness. During `vite dev`, the plugin adds local route hostnames to `allowedHosts`, binds to Tako's assigned port when `PORT` is set, reads fd-3 bootstrap before SSR code runs, and routes framework logs into Tako's log stream.

The Next.js adapter's `withTako()` adds `.test` and `.tako.test` to `allowedDevOrigins` and configures the Tako image loader.

## Diagnostics

```bash
tako doctor
```

Doctor checks daemon status, local DNS, TLS files, macOS proxy state, Linux routing setup, and startup hints. If the daemon is not running, doctor reports that state and exits successfully.
