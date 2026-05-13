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

1. It ensures local DNS, TLS, and proxy prerequisites are ready.
2. It starts `tako-dev-server` when needed.
3. It registers the selected config file with the daemon.
4. It starts the app process and waits for fd-4 readiness.
5. It streams logs and status into your terminal.

Running `tako dev` again for the same config attaches to the existing session when the app is running or idle.

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

Managed local DNS applies to `.test` and `.tako.test` routes. External routes are still routable by the local proxy but are not resolved by Tako DNS or advertised in LAN mode.

If explicit managed `.test` or `.tako.test` routes are configured, they replace the default `{app}.test` route. If only external routes are configured, Tako keeps the default managed route and adds the external routes.

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

## Keyboard Shortcuts

In an interactive terminal:

| Key      | Action                                     |
| -------- | ------------------------------------------ |
| `r`      | Restart the app process.                   |
| `l`      | Toggle LAN mode.                           |
| `b`      | Background the app and exit the CLI.       |
| `Ctrl-C` | Stop the app, unregister routes, and exit. |

No alternate screen is used, so normal terminal scrollback, search, selection, and clickable links keep working.

## Idle And Wake

The app starts immediately when `tako dev` starts. If there are no attached CLI clients for 30 minutes, the daemon can stop the process and keep routes registered as idle.

The next HTTP request wakes the app, waits for readiness, and then forwards the request.

Idle shutdown is suppressed while requests are in flight.

## Readiness

Tako sets:

```text
PORT=0
HOST=127.0.0.1
ENV=development
TAKO_DATA_DIR=...
TAKO_APP_NAME=...
TAKO_INTERNAL_SOCKET=...
```

The SDK binds an OS-assigned loopback port and writes the actual port to fd 4. A route becomes active only after this readiness handshake succeeds.

For Vite dev commands, use the `tako.sh/vite` plugin. Tako does not parse Vite stdout URLs as readiness; if a Vite-looking command never reports readiness, the CLI shows a Vite-specific plugin hint.

The same plugin reads Tako's fd 3 bootstrap before Vite SSR runs, so server code can sign image optimizer URLs with `createImageUrl()` from `tako.sh/server` during `tako dev`.

## Dev Commands

Tako resolves the dev command in this order:

1. top-level `dev` in `tako.toml`
2. preset `dev`
3. runtime default

Examples:

```toml
dev = ["vite", "dev"]
```

JavaScript runtime defaults run through the SDK HTTP entrypoints:

- Bun: `bun run node_modules/tako.sh/dist/entrypoints/bun-server.mjs {main}`
- Node: `node --experimental-strip-types node_modules/tako.sh/dist/entrypoints/node-server.mjs {main}`

Go defaults to:

```bash
go run .
```

Source hot reload belongs to the runtime or framework dev server. Tako watches configuration and platform definition files, not every source file.

## Watched Files

`tako dev` watches:

- `tako.toml`
- `.tako/secrets.json`
- `<app_root>/channels/`
- `<app_root>/workflows/`
- parent directories that can contain `tako.d.ts` (`app/`, `src/`, and the project root)

For JavaScript apps, `app_root` comes from `tako.toml` and defaults to `src`. Tako recreates `tako.d.ts` if the generated JS/TS declaration file is removed or edited. The app restarts when effective environment variables, secrets, channel definitions, or workflow definitions change. It updates dev routing without restarting when `[envs.development].route` or `routes` changes.

## Logs

Interactive logs render as:

```text
hh:mm:ss LEVEL [scope] message
```

Common scopes:

- `tako`: local dev daemon
- `app`: app process
- `worker`: workflow worker

App lifecycle changes appear inline in the stream. Attached sessions replay the shared log file, then follow new lines.

Dev logs live under:

```text
{TAKO_HOME}/dev/logs/{app}-{hash}.jsonl
```

## Local TLS

Tako creates a local root CA once per `{TAKO_HOME}`. The public cert and private key are stored at:

```text
{TAKO_HOME}/ca/ca.crt
{TAKO_HOME}/ca/ca.key
```

The private key is written with mode `0600`. On first run, or when trust is missing, Tako explains why elevated access is needed and installs the CA into the system trust store.

The daemon uses SNI to issue local leaf certificates for app domains.

## Local DNS And Proxy

### macOS

Tako writes split-DNS resolver files for:

- `/etc/resolver/test`
- `/etc/resolver/tako.test`

They point at the local DNS listener on `127.0.0.1:53535`.

Tako also installs a launchd-managed socket-activated proxy on `127.77.0.1`:

- `127.77.0.1:443 -> 127.0.0.1:47831`
- `127.77.0.1:80 -> 127.0.0.1:47830`

The user-facing URL has no explicit port on macOS.

### Linux

Tako configures systemd-resolved for `~test` and `~tako.test`, a loopback alias `127.77.0.1`, and local redirect rules for portless HTTPS. On NixOS, it prints a `configuration.nix` snippet instead of changing the system imperatively.

Non-macOS default URLs include the daemon HTTPS port when needed:

```text
https://{app}.test:47831/
```

## LAN Mode

Press `l` in `tako dev` to expose managed routes as `.local` aliases on your LAN.

Concrete hostnames are advertised with mDNS. Wildcard routes cannot be advertised because mDNS does not support wildcard records. The proxy can still match them for clients that resolve those names some other way.

External routes are not rewritten to `.local`, advertised with mDNS, or resolved by Tako DNS.

## Dev Subcommands

```bash
tako dev stop
tako dev stop my-app
tako dev stop --all
tako dev ls
```

`stop` unregisters routes and stops processes. `ls` lists registered dev apps. `list` is an alias for `ls`.

## Workflows In Dev

`tako dev` uses the same architecture as production: the dev daemon owns the run queue and spawns a separate worker process on demand.

Dev workers are scale-to-zero with a short idle timeout. They load fresh code on the next enqueue, so workflow edits do not require restarting the entire dev session.

If a worker exits non-zero before claiming work, the app is marked unhealthy for a short window and enqueue returns a visible error instead of silently queueing work that cannot run.

## Troubleshooting

Useful commands:

```bash
tako doctor
tako dev --verbose
tako dev stop --all
```

If daemon startup fails, Tako reports the last lines from:

```text
{TAKO_HOME}/dev-server.log
```

If a `.test` route does not resolve, check that the local CA, DNS listener, and platform proxy setup are healthy with `tako doctor`.
