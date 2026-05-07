---
layout: ../../layouts/DocsLayout.astro
title: "Self-hosted app deployment: server setup, rolling deploys, and scaling - Tako Docs"
heading: Deployment
current: deployment
description: "Guide to deploying apps with Tako on your own servers, including server setup, rolling deploys, scaling, secrets, and production operations."
---

# Deployment

This guide covers the production path: installing `tako-server`, registering servers, mapping environments, deploying releases, scaling instances, syncing secrets, and operating the service.

## Install the Server

Connect the host and your workstation to the same Tailscale tailnet, then run the server installer as root on each target host:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

The installer:

- creates the `tako` SSH/service user
- creates `tako-app` for app and worker processes
- installs `tako-server` to `/usr/local/bin/tako-server`
- installs systemd or OpenRC service files
- detects the host's Tailscale IP and binds remote management HTTP to port `9844`
- configures privileged bind support and app-user switching, failing on non-systemd/OpenRC hosts if file capabilities cannot be granted
- creates `/opt/tako` and `/var/run/tako`
- starts and verifies the service
- installs helpers needed for graceful reload and upgrade

Normal service installs require Tailscale because Tako keeps server control traffic private by default. If detection is not possible, set `TAKO_MANAGEMENT_HOST` to the server's Tailscale IP.

For GitHub-hosted release downloads, the installer uses `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.

Set `TAKO_SSH_PUBKEY` to install an SSH public key non-interactively:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo TAKO_SSH_PUBKEY="$(cat ~/.ssh/id_ed25519.pub)" sh
```

That key is authorized for `tako` SSH access and enrolled for signed remote management.

## Register the Server Locally

Add each server to your local global config:

```bash
tako servers add la.tailnet.ts.net --name la
tako servers add nyc.tailnet.ts.net --name nyc --description "New York"
tako servers add la.tailnet.ts.net --name la --install --admin-user root
```

The add command expects a Tailscale MagicDNS name or Tailscale IP. It verifies Tailscale resolution, `tako@host` SSH recovery access, signed private management HTTP, and the server target (`arch` and `libc`) before writing `config.toml`. Deploy requires that target metadata so it can choose the correct artifact.

Use `--install` when the host is new or `tako-server` needs repair. Tako connects as the admin SSH user, installs the server, enrolls the SSH key used for access, rechecks `tako@host`, verifies signed HTTP management, and only then saves the server locally. Without a host, `tako servers add` runs the same flow through an interactive wizard.

List configured servers:

```bash
tako servers ls
```

## Configure the Project

Map an environment to one or more servers:

```toml
name = "dashboard"
runtime = "bun"
preset = "tanstack-start"

[envs.production]
route = "dashboard.example.com"
servers = ["la", "nyc"]
```

Each non-development environment must define `route` or `routes`.

Routes can be exact hosts, wildcard hosts, or host plus path:

```toml
[envs.production]
routes = [
  "dashboard.example.com",
  "example.com/app/*",
  "*.example.com/admin/*"
]
```

`development` is reserved for `tako dev` and cannot be deployed.

## Deploy

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

`--env` defaults to `production`. Interactive production deploys require confirmation unless `--yes` or `-y` is set.

Deploy builds locally, ships artifacts to every server in the environment, prepares the release, runs the release command if configured, then rolls traffic to the new build.

If `[envs.production].servers` is empty and exactly one global server is configured, deploy can select it and write it into `tako.toml`. Otherwise, declare `servers` explicitly.

## Build and Artifact Contract

The deploy source root is the git root when available, otherwise the selected config file's parent directory.

Tako copies the source bundle into `.tako/build`, respecting `.gitignore`, symlinks local `node_modules` for build tools, runs configured build stages, verifies the runtime `main`, and archives the result.

Build stage precedence:

1. `[[build_stages]]`
2. `[build]`
3. runtime default
4. no-op

Always excluded from deploy artifacts:

- `.git/`
- `.tako/`
- `.env*`
- `node_modules/`

Additional excludes come from `[build].exclude`, per-stage `exclude`, and `.gitignore`.

Target artifacts are cached under `.tako/artifacts/` and validated by checksum and size before reuse. Deploy verifies the resolved runtime `main` exists in the build workspace before packaging.

## Runtime Preparation

Servers receive prebuilt artifacts; they do not run app build steps. After extracting an artifact, `tako-server` runs the runtime plugin's production install command, downloads or resolves the pinned runtime version when needed, and prepares the release directory.

Runtime definitions live in runtime plugins. Presets only supply metadata such as `main`, `assets`, and `dev`.

## Release Commands

Use `release` for work that must happen once before traffic shifts:

```toml
release = "bun run db:migrate"
```

Override or clear it per environment:

```toml
[envs.staging]
release = ""
```

The release command runs only on the leader server, inside the new release directory, after production dependency install and before rolling update. It receives app env, secrets, `TAKO_BUILD`, and `TAKO_DATA_DIR`.

If the command fails or times out after 10 minutes, deploy aborts on every server. Timed-out release commands are killed. The old release keeps serving.

## Rolling Updates

On each server, Tako:

1. starts a new instance
2. waits for health
3. adds it to the load balancer
4. drains an old instance
5. repeats until the target count is replaced
6. updates the `current` symlink

The rolling target count comes from server-side desired instance state. Deploy does not reset it.

If desired instances are `0`, deploy still keeps one warm instance for the new build so the app is reachable immediately after deploy. Later it can idle down.

If a new instance fails health checks, Tako kills the new process, keeps old instances serving, and reports the failure.

## Scaling

Scale every server in an environment:

```bash
tako scale 2 --env production
tako scale 0 --env production
```

Scale one server:

```bash
tako scale 3 --env production --server la
```

Outside a project directory:

```bash
tako scale 2 --app dashboard/production --server la
```

Desired counts persist across deploys, rollbacks, and server restarts. Scaling down drains in-flight requests before stopping excess instances.

## Secrets

Set local encrypted secrets:

```bash
tako secrets set DATABASE_URL --env production
tako secrets set API_KEY --env staging
```

Sync them to servers:

```bash
tako secrets sync
tako secrets sync --env production
```

Deploy compares a local secrets hash with the server's current hash. If unchanged, secrets are not resent. Fresh HTTP instances and workflow workers receive secrets through fd 3 at spawn time. Secret sync also refreshes workflow runtime and rolling-restarts HTTP instances so new processes receive updated values.

Secrets are stored encrypted in server SQLite. They are not written as plaintext `.env` files.

## TLS

Public routes use Let's Encrypt automatically. Certificates renew 30 days before expiry.

Private and local hostnames use self-signed certificates:

- `localhost`
- `*.localhost`
- single-label hosts
- `.local`
- `.test`
- `.invalid`
- `.example`
- `.home.arpa`

Wildcard routes require DNS-01 configuration:

```bash
tako servers setup-wildcard --env production
```

If a wildcard route is deployed without DNS provider configuration, deploy fails with guidance.

If no matching certificate exists yet, Tako serves a fallback self-signed certificate so HTTPS can complete and routing can return a normal HTTP response.

## Logs and Status

```bash
tako logs --env production
tako logs --env production --tail
tako logs --env production --json
tako servers status
tako releases ls --env production
```

`servers status` works from any directory and reports all configured servers through signed Tailscale HTTP management.

`logs` includes app output plus server lifecycle, health, and proxy diagnostics for the app's deployed routes. JS/TS production HTTP entrypoints route `console.*`, uncaught exceptions, and unhandled rejections into the same app log stream. Use `--json` for compact JSONL in agents and automation.

## Rollback

```bash
tako releases ls --env production
tako releases rollback abc1234 --env production --yes
```

Rollback reuses the selected release, current routes, env, secrets, and desired scaling state, then performs the standard rolling-update flow.

## Server Maintenance

Graceful reload:

```bash
tako servers restart la
```

Full restart:

```bash
tako servers restart la --force
```

Upgrade all servers or one server:

```bash
tako servers upgrade
tako servers upgrade la
```

Upgrade uses temporary process overlap and the management socket handoff so clients connect to the ready process.

Remove a server installation and all server-side data:

```bash
tako servers implode la
```

GitHub-backed upgrade metadata and remote archive downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.

## Data Layout

Production data lives under `/opt/tako`:

```text
/opt/tako/
  config.json
  identity.key
  identity.pub
  tako.db
  runtimes/
  certs/
  apps/
    {app}/{env}/
      current -> releases/{version}
      data/
        app/
        tako/
      logs/
      releases/{version}/
```

The management socket lives at:

```text
/var/run/tako/tako.sock
```

It is a symlink to the active PID-specific socket, which lets reloads hand off cleanly.

Each server also keeps a stable identity key at `/opt/tako/identity.key` and publishes its OpenSSH SHA-256 fingerprint through `hello` and `server_info`. Remote management requires Tailscale so Tako can keep server control traffic private by default; normal installs bind the private HTTP RPC listener to port `9844` on the Tailscale address. Signed HTTP management keys are stored in `/opt/tako/management-authorized-keys`.

## Common Failure Behavior

- insufficient disk space fails before upload
- missing server target metadata fails before deploy
- concurrent deploys for the same app fail immediately
- failed release commands abort before traffic shifts
- failed warm startup keeps old instances serving
- failed partial releases are cleaned up automatically
