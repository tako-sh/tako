---
layout: ../../layouts/DocsLayout.astro
title: "Troubleshooting deploy failures, TLS issues, and runtime errors - Tako Docs"
heading: Troubleshooting
current: troubleshooting
description: "Troubleshoot common Tako problems including deploy failures, TLS issues, runtime errors, server status problems, and verbose diagnostics."
---

# Troubleshooting

Start with these commands:

```bash
tako doctor
tako servers status
tako logs --env production
tako deploy --verbose
```

Use `--ci` when reproducing a problem in automation. It disables prompts, colors, and spinners.

## `tako dev` Does Not Start

Run:

```bash
tako doctor
```

Doctor checks the dev daemon, DNS, loopback setup, and local proxy state.

If the daemon is missing, run:

```bash
tako dev
```

If daemon startup fails, Tako reports the last lines from `{TAKO_HOME}/dev-server.log`.

## Local HTTPS Fails

Check the URL first:

```text
https://{app}.test/
```

On macOS, Tako always uses a portless URL. On Linux, Tako configures redirects so the same URL works.

Run:

```bash
tako doctor
```

Common causes:

- root CA is not trusted yet
- local DNS resolver setup is missing
- macOS launchd proxy is not loaded
- port 80 or 443 forwarding is unavailable
- another resolver owns `/etc/resolver/test`

If `.test` conflicts with existing resolver config, try the `.tako.test` fallback route.

## Vite Dev App Never Becomes Ready

Tako waits for fd-4 readiness. It does not parse Vite stdout URLs.

Use the Vite plugin:

```ts
import { tako } from "tako.sh/vite";

export default {
  plugins: [tako()],
};
```

If your `dev` command runs Vite directly, keep it as an array:

```toml
dev = ["vite", "dev"]
```

## Dev Route Is Not Registered

Check `[envs.development]`:

```toml
[envs.development]
routes = ["dashboard.test", "api.dashboard.test"]
```

Development routes may use `.test`, `.tako.test`, or external hostnames. Tako only manages DNS for `.test` and `.tako.test`; external hostnames must be pointed at the dev proxy yourself. Wildcard dev routes participate in proxy routing, but cannot be advertised with mDNS in LAN mode.

If no development route is configured, Tako uses `{app}.test`. If only external development routes are configured, Tako keeps `{app}.test` and adds the external routes as host aliases.

## Deploy Says No Server Is Configured

Add and map a server:

```bash
tako servers add 203.0.113.10 --name la
```

Then in `tako.toml`:

```toml
[envs.production]
route = "dashboard.example.com"
servers = ["la"]
```

If exactly one server exists and production has no server mapping, interactive deploy can offer to write it for you.

## Deploy Fails With Missing Target Metadata

Deploy requires `arch` and `libc` metadata in `config.toml`.

Re-add the server with SSH checks enabled:

```bash
tako servers rm la
tako servers add 203.0.113.10 --name la
```

Avoid `--no-test` unless you know the metadata is already present.

## Deploy Cannot Find `main`

Tako resolves the runtime entrypoint in this order:

1. `main` in `tako.toml`
2. manifest main such as `package.json` `main`
3. preset `main`
4. JavaScript index fallback for supported index-style presets

Fix by setting `main`:

```toml
main = "dist/server/tako-entry.mjs"
```

For TanStack Start, ensure `tako.sh/vite` emits `dist/server/tako-entry.mjs`.

For Next.js, ensure `withTako()` emits `.next/tako-entry.mjs`.

## Production Deploy Asks for Confirmation

Production deploys prompt by default:

```bash
tako deploy --env production --yes
```

Use `--ci` in automation so missing prompts become explicit errors.

## Another Deploy Is Already Running

Tako serializes deploys per app and environment on each server.

Wait for the running deploy to finish, then retry:

```bash
tako deploy --env production
```

If `tako-server` restarted during an old deploy, retrying is safe. In-memory deploy locks are cleared by restart.

## Release Command Failed

Release commands run once on the leader server before rolling update:

```toml
release = "bun run db:migrate"
```

On failure, deploy aborts before traffic shifts and old instances keep serving.

Debug with:

```bash
tako deploy --env production --verbose
tako logs --env production
```

The release command runs in the new release directory with app env, secrets, `TAKO_BUILD`, and `TAKO_DATA_DIR`.

## App Fails Health Checks

Tako probes:

```http
GET /status
Host: tako.internal
```

Use the SDK wrapper for your runtime so the endpoint and readiness protocol are installed.

Health startup timeout is 30 seconds. If startup does not produce a healthy instance, deploy rolls back. For scale-to-zero cold starts, the proxy can return:

- `504 App startup timed out`
- `502 App failed to start`
- `503 App startup queue is full`

## TLS Certificate Problems

For public hostnames, confirm DNS points to the server and port 80 is reachable for HTTP-01 challenges.

For wildcard routes, configure DNS-01:

```bash
tako servers setup-wildcard --env production
```

If no matching certificate exists yet, Tako serves a fallback self-signed certificate so HTTPS can complete and routing can return a normal HTTP response.

## Secrets Are Missing in Production

List local secrets:

```bash
tako secrets ls
```

Sync to servers:

```bash
tako secrets sync --env production
```

Deploy also sends secrets when the server hash differs. Secret values are delivered to long-running app and worker processes through fd 3 at spawn time, so existing processes need a restart to receive new values. `secrets sync` triggers the required refresh.

## Rollback Needed

List releases:

```bash
tako releases ls --env production
```

Roll back:

```bash
tako releases rollback abc1234 --env production --yes
```

Rollback uses the current routes, env, secrets, and desired scaling state, then performs rolling update.

## Server Status Is Empty

Run:

```bash
tako servers ls
tako servers status
```

`servers status` reads global `config.toml`; it does not require a project directory. If no servers are configured, add one with `tako servers add`.

## Disk Space Failure

Deploy checks free space under `/opt/tako` before uploading artifacts.

Free space or remove old data, then retry. Tako also prunes local artifact cache best-effort, but remote disk cleanup is an operator action.

## Config Parse Errors

Run with the selected config explicitly:

```bash
tako deploy -c tako.staging.toml --verbose
```

Common validation issues:

- unknown top-level key
- both `route` and `routes` in one environment
- non-development environment without routes
- unsupported runtime
- namespaced preset in `tako.toml`
- absolute asset or build paths
- `idle_timeout = 0`

## Getting More Detail

Use:

```bash
tako <command> --verbose
tako <command> --ci --verbose
```

Verbose mode shows timestamps and debug lines. CI verbose mode keeps output deterministic for logs.
