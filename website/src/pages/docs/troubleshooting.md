---
layout: ../../layouts/DocsLayout.astro
title: "Troubleshooting deploy failures, TLS issues, and runtime errors - Tako Docs"
heading: Troubleshooting
current: troubleshooting
description: "Troubleshoot common Tako problems including deploy failures, TLS issues, runtime errors, server status problems, and verbose diagnostics."
---

# Troubleshooting

Start with a current snapshot:

```bash
tako doctor
tako servers status
tako logs --env production
tako deploy --verbose
```

Use deterministic output in automation:

```bash
tako deploy --ci --verbose
tako logs --env production --json
```

Progress, prompts, and logs go to stderr. Machine-readable results go to stdout.

## Config Not Found

App-scoped commands read `./tako.toml` by default:

```bash
tako deploy
```

Choose another config file with `-c`:

```bash
tako deploy -c apps/web/tako.production
```

The `.toml` suffix is optional. The selected file's parent directory becomes the app directory.

Commands that honor `-c` include `init`, `dev`, `logs`, `deploy`, `releases`, `delete`, `secrets`, `storage`, `generate`, and project-context `scale`.

## Invalid App Or Server Name

App names and server names must:

- start with a lowercase letter
- contain only lowercase letters, numbers, and hyphens
- end with a lowercase letter or number
- be 63 characters or fewer

Valid:

```toml
name = "dashboard"
```

Invalid:

```toml
name = "Dashboard"
name = "dashboard-"
name = "dash_board"
```

If `name` is omitted, Tako derives it from the selected config file's parent directory. Set `name` before deploying long-lived apps so directory moves do not create new server-side app identities.

## Environment Not Found

Deploy, logs, releases, delete, and scale resolve an environment. `production` is the default.

```toml
[envs.production]
route = "dashboard.example.com"
servers = ["la"]
```

`development` is reserved for `tako dev` and cannot be deployed.

## Route Problems

Use either `route` or `routes`, not both:

```toml
[envs.production]
routes = [
  "example.com",
  "*.example.com/admin/*",
  "example.com/api/*",
]
```

Non-development environments must define at least one route. Route config belongs under `[envs.<env>]`; environment variables belong under `[vars]` and `[vars.<env>]`.

If deploy reports a route conflict, another app on that server already claims an overlapping host/path. Run:

```bash
tako servers status
```

Then adjust one app's routes or delete the old deployment.

## No Servers Configured

Add a server:

```bash
tako servers add host.example.com --name la
```

If the host does not have `tako-server` installed yet:

```bash
tako servers add ubuntu@host.example.com --install --name la
```

`admin-user@host` tells Tako which admin SSH user to use for install or repair. The server is stored locally as just the host.

## Target Metadata Missing

Deploy requires each selected server to have target metadata:

- `arch`: `x86_64` or `aarch64`
- `libc`: `glibc` or `musl`

`tako servers add` detects and stores this metadata. If deploy says metadata is missing or invalid, remove and re-add the server with SSH checks enabled:

```bash
tako servers rm la
tako servers add host.example.com --name la
```

## SSH Or Remote Management Fails

Tako uses local SSH keys first, then `ssh-agent` when available. If your key is passphrase-protected, run interactively or pass:

```bash
tako servers status --ssh-passphrase "$PASSPHRASE"
```

Normal remote management requires Tailscale. Server setup must configure private management HTTP on the Tailscale address. `tako servers add` verifies:

- host resolves to a Tailscale address
- `tako@host` SSH recovery works
- the SSH key is enrolled for signed management
- unsigned `hello` / `server_info` probes work
- signed management RPC works

If any check fails, the server is not written to `config.toml`.

## Deploy Fails Before Upload

Common early deploy failures:

- target environment is missing
- target environment has no route
- server names are not in global `config.toml`
- server target metadata is missing
- required secret keys are unavailable locally
- disk space under `/opt/tako` is insufficient
- another deploy already holds the project-local `.tako/deploy.lock`

Run:

```bash
tako deploy --env production --verbose
```

Verbose mode shows each step as an append-only transcript.

## Build Or Entrypoint Fails

Deploy resolves `main` in this order:

1. top-level `main` in `tako.toml`
2. runtime manifest main field such as `package.json` `main`
3. preset `main`

For JS index-style presets, Tako also checks common root and `src/` entrypoints.

If deploy says the entrypoint is missing, either update `main`, choose the right preset, or make sure your build emits the expected file:

```toml
main = "dist/server/tako-entry.mjs"
```

For Vite and TanStack Start apps, use `tako.sh/vite` so the deploy entry wrapper is emitted during build.

## Release Command Fails

`release` runs once on the leader server after extraction and production install, before rolling update:

```toml
release = "bun run db:migrate"
```

If it exits non-zero, times out, or is killed, deploy aborts everywhere. The new partial release directory is removed, `current` is not updated, and old instances keep serving.

Use logs and verbose deploy output to inspect stderr tails:

```bash
tako deploy --verbose
tako logs --env production
```

## App Starts Then Returns 502 Or 504

`502 Bad Gateway` during cold start usually means startup failed before readiness. `504 Gateway Timeout` means no healthy instance became ready before the startup timeout.

Check app logs:

```bash
tako logs --env production --tail
```

For JS apps, make sure the app is running under the SDK entrypoint or framework adapter and writes readiness through fd 4. For Vite dev commands, install and configure the `tako.sh/vite` plugin.

## Dev DNS Or HTTPS Fails

Run:

```bash
tako doctor
```

On macOS, doctor checks:

- dev proxy install status
- boot helper load status
- loopback alias `127.77.0.1`
- launchd service status
- reachability on `127.77.0.1:443` and `127.77.0.1:80`

On Linux, `tako dev` configures loopback, iptables redirects, and systemd-resolved rules. On NixOS, Tako prints a `configuration.nix` snippet instead of applying imperative setup.

If `https://{app}.test/` fails but the daemon is reachable on `127.0.0.1:47831`, the local proxy or DNS setup is the likely problem.

## Dev App Is Idle Or Stopped

`tako dev` apps can be:

- `running`: actively serving
- `idle`: process stopped, routes retained for wake-on-request
- `stopped`: unregistered, routes removed

List sessions:

```bash
tako dev list
```

Stop one session:

```bash
tako dev stop
tako dev stop my-app
tako dev stop --all
```

## Secrets Are Missing

Secrets live in `.tako/secrets.json`; keys live outside the repo.

Set and sync:

```bash
tako secrets set DATABASE_URL --env production --sync
tako secrets sync --env production
```

Export/import keys for another machine:

```bash
tako secrets key export --env production
tako secrets key import --env production
```

On macOS, interactive key creation/import can use iCloud Keychain when running through the signed `Tako.app` CLI. If the signed app entitlement is unavailable, keychain writes fail before changing local secret files.

## Workflow Enqueue Fails In Dev

In dev, workflow workers are scale-to-zero. If a worker exits non-zero before claiming any run, the supervisor marks it unhealthy for a short window and enqueue returns a clear worker error instead of silently queuing work.

Check the same `tako dev` log stream. Worker output is scoped as `worker`.

Make sure workflow files live in:

```text
<app_root>/workflows/
```

`app_root` defaults to `src`.

## Channel Connection Fails

Channel routes are flat:

```text
/_tako/channels/<name>
```

Dynamic values belong in query params validated by the channel's `paramsSchema`.

For auth-required WebSocket channels, the first text frame must be the Tako auth frame within five seconds:

```json
{ "type": "tako.auth", "token": "Bearer ...", "lastMessageId": "123" }
```

SSE clients resume with `Last-Event-ID`; WebSocket clients resume with `last_message_id`.

## Image URLs Fail

Public optimized image URLs use:

```text
/_tako/image?src=/assets/hero.jpg&w=1200
```

Failures commonly come from:

- missing `src` or `w`
- duplicate or unknown query params
- unsupported width, quality, or format
- local path blocked by `[images].local_patterns`
- remote URL not matching `[images].remote_patterns`
- remote source with redirects, private IPs, unsupported schemes, userinfo, or fragments
- source bytes exceeding optimizer limits

Image optimizer failures return non-shared error caching and detailed diagnostics go to app logs. If a storage image URL or srcset fails, check that `tako storages add` configured the right environment and that `--public-base-url` is set when using `createImageUrl(..., { public: true })` or `createImageSrcSet(..., { public: true })`.

## Logs Are Hard To Read

History mode defaults to the last three days:

```bash
tako logs --env production
```

Stream live logs:

```bash
tako logs --env production --tail
```

Emit JSONL:

```bash
tako logs --env production --json
```

When multiple servers are targeted, human logs are prefixed by server name and sorted by timestamp.
