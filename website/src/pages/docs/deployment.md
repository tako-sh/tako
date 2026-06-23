---
layout: ../../layouts/DocsLayout.astro
title: "Self-Hosted Deployment - Tako Docs"
heading: Deployment
current: deployment
description: "Guide to deploying apps with Tako on your own servers, including server setup, rolling deploys, scaling, secrets, and production operations."
---

# Deployment

Tako deploys locally built apps to servers you control. The CLI validates project state, builds and packages the release, uploads artifacts over signed private HTTP management, and asks `tako-server` to prepare and roll the release into traffic.

## Server Setup

Install the server on a Linux host:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

Custom public ports:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo sh -s -- --http-port 8080 --https-port 8443
```

The installer creates the `tako` service user, the shared `tako-app` group, `/opt/tako`, `/var/run/tako`, service files, maintenance helpers, restricted sudoers policy, public HTTP/HTTPS listeners, local metrics, libvips runtime support, and private Tailscale management.

Normal installs require Tailscale for remote management. The CLI expects a Tailscale MagicDNS host or Tailscale IP, verifies SSH recovery access, enrolls the SSH key for signed management, and then uses signed HTTP for app operations.

## Add A Server

```bash
tako servers add my-server.tailnet.ts.net --install
```

or:

```bash
tako servers add ubuntu@my-server.tailnet.ts.net
```

`--install` and `admin@host` flows install or repair `tako-server` before writing local `config.toml`. The add flow stores host, SSH port, public HTTP/HTTPS ports, description, and detected target metadata (`arch`, `libc`).

## Configure An Environment

```toml
[envs.production]
route = "app.example.com"
servers = ["la"]
```

Deploy targets the servers listed in `[envs.<env>].servers`. If production has no server mapping and exactly one global server exists, deploy can select it and persist the mapping. Other non-development environments require explicit server mappings.

## Deploy

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

When `--env` is omitted, deploy targets `production`. In an interactive terminal, an implicit production deploy asks for confirmation. Passing `--env production`, `--yes`, or `-y` makes the target explicit.

Deploy validates secrets, storage credentials, provider credentials, routes, server target metadata, workflow/channel storage requirements, and local-storage limitations before build work starts.

## Build And Artifact Rules

Build precedence:

1. `[[build_stages]]`
2. `[build]`
3. Runtime default
4. No-op

Tako copies the source root into `.tako/build`, respects `.gitignore`, symlinks `node_modules` from the original tree for JS builds, preserves symlinks as symlinks, runs build commands, merges assets into `public/`, verifies the resolved `main`, and archives the result without `node_modules`.

Always-excluded paths: `.git/`, `.tako/`, `.env*`, and `node_modules/`.

Version names are based on git state: clean commit hash, dirty commit plus content hash, or `nogit_<hash>` when no git commit is available.

## Native Releases

Native releases use runtime plugins or explicit `start` commands. JS runtimes run through SDK entrypoint wrappers. Go binaries run directly. Explicit `start` commands skip runtime defaults and runtime version probing.

Native HTTP instances bind `127.0.0.1` on an OS-assigned port and signal readiness through fd 4. Secrets, storage bindings, and the internal health token arrive through fd 3.

## Container Releases

```toml
container = "Dockerfile"
```

Container releases upload source and let `tako-server` build the image with Podman. HTTP containers run from the image defaults and receive `HOST=0.0.0.0`, `PORT=3000`, app vars, `TAKO_BUILD`, and `TAKO_BOOTSTRAP_DATA`.

Use a Tako SDK inside the container so secrets, storage bindings, internal status, and health-probe authentication follow the same contract as native releases. In v0, container HTTP instances do not receive fd 3, fd 4, the internal socket, or `TAKO_DATA_DIR`.

A configured workflow `run` starts a separate container process from the same image with an entrypoint override, args from `run[1..]`, a mounted internal socket, and `TAKO_BOOTSTRAP_DATA`. In v0, container releases support one configured workflow `run` command across the base workflow config and named groups.

## Release Commands

```toml
release = "bun run db:migrate"

[envs.staging]
release = ""
```

The release command runs once on the leader server after extract and production install but before rolling update. Followers wait for the leader result. If the command fails, times out, or exits by signal, deploy aborts on every server and leaves the old release serving.

The command runs with the same env as new HTTP instances, plus freshly decrypted secrets as env vars. The hard timeout is 10 minutes.

## Rolling Updates

For each server, Tako starts replacement instances, waits for health, keeps the new batches out of public request routing until each stays healthy for a short stability window, then routes traffic to the stable replacement set and drains the old instances. The `current` symlink moves only after the rolling update succeeds.

During validation, a server may temporarily run both the old instance set and the new replacement set.

After finalize, each server keeps the active release and prunes non-active releases older than 30 days or beyond 50 total releases.

If desired instances are `0`, deploy still starts one warm instance for the new build so traffic works immediately. It can later idle out.

Failures, including a new instance becoming unhealthy during the stability window, keep old instances running, clean partial release directories when needed, roll back release metadata, and report per-server results.

## Scaling

```bash
tako scale 3 --env production
tako scale 0 --env production
```

Desired instances are server-side state. They persist across deploys, rollbacks, and server restarts. Scaling above the effective server maximum fails. Scaling to zero enables cold starts after idle timeout.

## Secrets And Credentials

App secrets:

```bash
tako secrets set DATABASE_URL --env production
tako secrets sync --env production
```

Provider credentials:

```bash
tako credentials set ssl.cloudflare --env production
tako credentials set postgres_url --env production
```

Deploy sends app secrets only when the server's current secret hash differs. Provider credentials are sent only for the runtime binding that needs them, such as Cloudflare SSL, Let's Encrypt DNS-01, or shared channel/workflow storage.

## Storage And Backups

Attach app storage:

```bash
tako storages add uploads --env production --provider s3 --bucket app-uploads --endpoint https://example.r2.cloudflarestorage.com --region auto
```

Set backup-only credentials:

```bash
tako storages credentials private_backups --env production
```

Backups are enabled in `tako.toml` with `backup = { storage = "resource" }`. Deploy sends backup storage separately from app storage, and it is not exposed through SDK storage bindings unless also listed in `[envs.<env>].storages`. Backup archives preserve symlinks as symlinks.

## TLS

Let's Encrypt is the default. Exact routes can use HTTP-01; wildcard routes require Cloudflare DNS-01 with `ssl.cloudflare`. `ssl = "cloudflare"` uses Cloudflare Origin CA and also requires `ssl.cloudflare`.

If public HTTPS uses a non-default port, deploy summaries include that port and HTTP redirects target it.

## Server Maintenance

```bash
tako servers reload la
tako servers reload la --force
tako servers upgrade la
tako servers uninstall la --yes
```

`reload` is zero-downtime by default. `--force` performs a restart. `upgrade` installs a new `tako-server` binary, enters upgrade mode, reloads, waits for readiness, and rolls back to the previous binary if readiness fails. `uninstall` removes the remote service and data, then removes the local server entry.

## Logs And Releases

```bash
tako logs --env production
tako logs --env production --tail
tako logs --env production --json
tako releases list --env production
tako releases rollback <release-id> --env production
```

Logs include app stdout/stderr and app-scoped server diagnostics. Releases show merged release history across mapped servers and mark the current release.
