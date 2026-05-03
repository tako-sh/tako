---
layout: ../../layouts/DocsLayout.astro
title: "Local development with Tako dev: HTTPS, domains, and hot reload - Tako Docs"
heading: Development
current: development
description: "Learn how tako dev provides trusted HTTPS, custom .test domains, hot reload, variants, and a persistent local background daemon."
---

# Local Development with Tako

`tako dev` runs your app behind trusted local HTTPS on a `.test` hostname:

```bash
tako dev
```

Default URL:

```text
https://{app}.test/
```

The app name comes from `name` in `tako.toml`, or from the selected config file's parent directory.

## How It Works

`tako dev` is a client for a persistent local daemon called `tako-dev-server`.

When you run it, the CLI:

1. starts the daemon if needed
2. registers the selected config file
3. resolves runtime, preset, env, routes, and dev command
4. starts or attaches to the app process
5. streams logs and lifecycle events

The selected config file is the unique app key. Running `tako dev` again from the same config attaches to the existing session when possible.

## Hostnames and Routes

If `[envs.development]` is not configured, Tako registers:

```text
https://{app}.test/
```

Use explicit dev routes when needed:

```toml
[envs.development]
routes = ["dashboard.test", "api.dashboard.test"]
```

Configured `.test` and `.tako.test` routes replace the default route. External routes are additive: if you only configure external hostnames, Tako still keeps `{app}.test` and also routes those hostnames. External hostnames must be pointed at the dev proxy yourself, for example with a tunnel or DNS rule.

Both `.test` and `.tako.test` resolve through Tako's local DNS. `.tako.test` remains available as a fallback zone. Wildcard dev routes participate in proxy routing, but cannot be advertised with mDNS in LAN mode.

## Variants

Run multiple variants of the same app:

```bash
tako dev --variant preview
tako dev --var preview
```

The hostname becomes:

```text
https://{app}-preview.test/
```

## Dev Command Resolution

Tako chooses the dev command in this order:

1. `dev` in `tako.toml`
2. preset `dev`
3. runtime default

Example override:

```toml
dev = ["vite", "dev"]
```

Vite dev commands must use the `tako.sh/vite` plugin so Vite can write readiness to fd 4. Tako does not parse Vite stdout URLs as readiness.

## Readiness

The app is considered running only after the process writes its bound loopback port to fd 4.

Until then, routes are not activated. If the process exits, the route goes idle and the next request can wake it again.

## Logs and Status

Interactive `tako dev` prints a normal terminal log stream. It does not use an alternate screen.

Log lines are formatted with:

- timestamp
- level
- scope
- message

Common scopes are `tako`, `app`, and `worker`.

Shared logs are stored under Tako's home directory so attached sessions can replay previous output and follow new output.

## Keyboard Shortcuts

| Key      | Action                                     |
| -------- | ------------------------------------------ |
| `r`      | Restart the app process.                   |
| `l`      | Toggle LAN mode.                           |
| `b`      | Background the app and exit the CLI.       |
| `Ctrl-C` | Stop the app, unregister routes, and exit. |

## Backgrounding

Press `b` to leave the app running under the daemon. Later, run:

```bash
tako dev
```

to attach again.

After 30 minutes with no attached CLI clients, the app can become idle. The next HTTP request wakes it.

## Stop and List

```bash
tako dev stop
tako dev stop dashboard
tako dev stop --all
tako dev ls
tako dev list
```

`stop` without a name stops the app for the selected config file.

## Local HTTPS

Tako generates a local root CA on first use. The CA private key is stored in the system keychain, scoped per Tako home directory. The public CA certificate is available at:

```text
{TAKO_HOME}/ca/ca.crt
```

Tako installs the CA into the system trust store when needed. Before prompting for elevated access, it explains what will change.

Leaf certificates are generated on demand for app hostnames and selected by SNI.

## macOS Networking

On macOS, Tako uses:

- a dedicated loopback alias: `127.77.0.1`
- a launchd-managed dev proxy for `:80` and `:443`
- split DNS resolver files for `.test` and `.tako.test`
- a local DNS listener on `127.0.0.1:53535`

The public app URL is portless:

```text
https://dashboard.test/
```

If an existing `/etc/resolver/test` was not created by Tako, Tako leaves it alone and warns. `.tako.test` still works as a fallback.

## Linux Networking

On Linux, Tako uses:

- the same dedicated loopback alias
- iptables redirect rules for 443, 80, and 53
- systemd-resolved routing for `~test` and `~tako.test`

On NixOS, Tako prints a `configuration.nix` snippet instead of applying imperative setup.

## LAN Mode

Press `l` in interactive mode to expose registered `.test` and `.tako.test` dev routes through `.local` aliases on the local network.

Concrete managed hostnames are advertised with mDNS. External routes are not rewritten to `.local` or advertised. Wildcard routes cannot be advertised by mDNS, so Tako warns and suggests explicit subdomain routes.

## Environment

`tako dev` loads:

1. `[vars]`
2. `[vars.development]`
3. runtime vars

It sets:

- `ENV=development`
- `TAKO_DATA_DIR`
- `NODE_ENV=development` for JavaScript runtimes
- `BUN_ENV=development` for Bun

Secrets are read from `.tako/secrets.json` and delivered through the same runtime path used by production.

## Workflows in Dev

Dev mode uses the same workflow architecture as production. The dev daemon owns the runs database and spawns a worker subprocess on demand.

Workers are scale-to-zero in dev, with a short idle timeout. Code edits are picked up on the next enqueue because the worker is spawned fresh.

If a worker crashes before claiming work, enqueues fail loudly for a short unhealthy window instead of silently queueing work that cannot run.

## Diagnostics

Run:

```bash
tako doctor
```

It checks the daemon, local DNS, loopback alias, macOS proxy state, and port reachability. If the daemon is not running, doctor reports that and exits successfully.
