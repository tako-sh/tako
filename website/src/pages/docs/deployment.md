---
layout: ../../layouts/DocsLayout.astro
title: "Self-hosted app deployment: server setup, rolling deploys, and scaling - Tako Docs"
heading: Deployment
current: deployment
description: "Guide to deploying apps with Tako on your own servers, including server setup, rolling deploys, scaling, secrets, and production operations."
---

# Deployment

Tako deploys locally built artifacts to servers you control. The CLI builds, packages, uploads, and orchestrates. `tako-server` extracts the release, installs production dependencies, runs optional release commands, manages TLS, starts app processes, and rolls traffic forward.

## Server Setup

Bootstrap `tako-server` on each host:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

The host installer installs the binary, service users, maintenance helpers, and service definition, but it does not enable or start `tako-server` by default. `tako servers add` configures remote management, enables the service, starts it, verifies access, and then stores the server locally.

Custom public proxy ports can be passed during bootstrap:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo sh -s -- --http-port 8080 --https-port 8443
```

Then register the host locally:

```bash
tako servers add host.example.com --name la
```

If the server is missing or needs repair:

```bash
tako servers add ubuntu@host.example.com --install --name la
```

The installer:

- creates `tako` for SSH and the server service
- creates `tako-app` for app and worker processes
- installs `tako-server` to `/usr/local/bin/tako-server`
- creates `/opt/tako` and `/var/run/tako`
- prepares those roots without recursively traversing existing app releases
- installs systemd or OpenRC service files without enabling or starting them
- installs libvips for image optimization
- configures private Tailscale remote management during `tako servers add`
- enrolls the SSH key for signed management

Configure/start mode requires Tailscale for private control traffic. If no Tailscale IP is available when `tako servers add` configures the service, the command fails with a remote-management hint.

If public traffic reaches Tako through another proxy or load balancer, run:

```bash
tako servers configure <name>
```

Choose source-IP configuration. PROXY protocol v1/v2 is for TCP proxies such as same-host HAProxy. Cloudflare orange-cloud HTTP proxying uses `CF-Connecting-IP`, not PROXY protocol. In both cases, configure trusted proxy CIDRs and only enable source-IP trust when clients cannot reach Tako directly around that proxy.

## Server Inventory

Server inventory is global user config, not project config. It lives in the platform config directory as `config.toml`.

```toml
[[servers]]
name = "la"
host = "la.example.ts.net"
port = 22
http_port = 80
https_port = 443
description = "Primary LA host"
arch = "x86_64"
libc = "glibc"
```

`tako servers add` detects and stores target metadata (`arch`, `libc`) and public proxy ports. Deploy requires valid metadata for every selected server.

Server names use the same validation rules as app names: lowercase letters, numbers, hyphens, start with a lowercase letter, end with a letter or number, and at most 63 characters.

## Configure Environments

Map project environments to server names in `tako.toml`:

```toml
name = "dashboard"

[envs.production]
route = "dashboard.example.com"
servers = ["la", "nyc"]
idle_timeout = 300

[envs.staging]
routes = ["staging.example.com", "example.com/staging/*"]
servers = ["staging"]
```

The deployment identity on each server is `{app}/{env}`. The same physical server can host multiple environments for the same app because each environment gets a separate identity and filesystem path.

`development` is reserved for `tako dev` and cannot be deployed.

## Deploy

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

`--env` defaults to `production`. Interactive production deploys ask for confirmation only when the environment is implicit. Passing `--env production` or `--yes` makes the target explicit and skips the prompt.

Deploy requires:

- a declared target environment
- `route` or `routes`
- valid server names
- target metadata for every server
- local secret keys for required secrets
- enough free disk space under `/opt/tako`

If `production` has no server mapping and exactly one global server exists, interactive deploy can write that server into `[envs.production].servers`.

## Build And Package

Deploy resolves the source root from the git root when available, otherwise from the app directory. The app subdirectory is the selected config file's parent directory relative to that source root.

Tako copies source into `.tako/build`, respects `.gitignore`, links `node_modules` from the original tree, runs build stages, merges assets into `public/`, verifies the resolved `main`, writes `app.json`, and archives the result.

Force-excluded paths:

- `.git/`
- `.tako/`
- `.env*`
- `node_modules/`

Additional excludes come from `[build].exclude`, per-stage excludes, and `.gitignore`.

Build stage precedence:

1. `[[build_stages]]`
2. `[build]`
3. runtime default
4. no-op

Built target artifacts are cached under `.tako/artifacts/`. Cache entries are verified by checksum and size before reuse, and invalid entries are rebuilt. Deploy prunes old cached target artifacts on a best-effort basis.

## Runtime Metadata

`app.json` is the canonical runtime manifest in each release. It includes:

- resolved runtime
- resolved `main`
- package manager
- runtime version
- non-secret environment variables
- JS `app_root`
- public image optimizer config
- environment idle timeout
- release metadata such as commit message and dirty state

If `runtime_version` is set in `tako.toml`, deploy uses it directly. Otherwise, deploy runs `<runtime> --version` and falls back to `latest`.

After extraction, `tako-server` runs the runtime plugin's production install command. It does not run app build steps on the server.

## Release Commands

Use `release` for one-shot work such as migrations:

```toml
release = "bun run db:migrate"

[envs.staging]
release = ""
```

The command runs once on the leader server, which is the first server in `[envs.<env>].servers`. Followers wait for the result before rolling update.

Release command behavior:

- cwd is the new release directory
- command runs as `sh -c "<command>"`
- env matches the new app instance env
- secrets are injected as env vars for that one-shot command
- timeout is 10 minutes

If the command fails, deploy aborts everywhere, removes the new partial release directory, leaves `current` unchanged, and old instances keep serving.

## Rolling Updates

On each server, rolling update:

1. Starts a new instance.
2. Waits for health checks.
3. Adds it to the load balancer.
4. Drains and stops an old instance.
5. Repeats until the target count is on the new release.
6. Updates `current` to the new release.
7. Cleans up releases older than 30 days.

Health checks use:

```text
GET /status
Host: <app>.tako
X-Tako-Internal-Token: <instance-token>
```

The SDK implements the status endpoint and echoes the token.

If startup fails during a warm deploy, deploy fails. If a later cold start fails, users receive generic `502` or `504` responses while details go to the app log stream.

## Scaling

Desired instance count is stored on each server:

```bash
tako scale 2 --env production
tako scale 0 --env production
```

`N > 0` keeps at least `N` healthy instances running. `0` enables scale-to-zero: deploy keeps one warm instance initially, then idle instances stop after `idle_timeout`. The next request wakes the app and waits for readiness.

Scale settings survive server restarts, deploys, and rollbacks.

## Secrets

Local secrets are encrypted in `.tako/secrets.json`. Keys live outside the repo and can be exported/imported:

```bash
tako secrets set DATABASE_URL --env production --sync
tako secrets key export --env production
tako secrets key import --env production
```

Deploy compares a server secrets hash before sending secrets. If hashes match, secrets are omitted and the server keeps its current encrypted values. If they differ, deploy sends decrypted secrets over the signed management path and `tako-server` stores them encrypted in SQLite.

Fresh HTTP instances and workflow workers receive secrets through fd 3. Secret syncs trigger worker restart and HTTP rolling restart.

## Storage

Attach object storage before deploy:

```bash
tako storages add uploads \
  --env production \
  --provider r2 \
  --bucket app-uploads \
  --endpoint https://<account>.r2.cloudflarestorage.com \
  --region auto
```

Local storage credentials are encrypted in `.tako/storages.json`. Deploy decrypts the selected environment's storage bindings locally, sends them over the signed management path, and `tako-server` stores them encrypted in SQLite.

Fresh HTTP instances and workflow workers receive storage bindings through fd 3 alongside secrets. In JavaScript apps, use `tako.storages.<name>.createDownloadUrl`, `createUploadUrl`, `createImageUrl`, and `createImageSrcSet`.

## Images

Public optimized images are configured in `tako.toml`:

```toml
[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
```

The optimizer endpoint is `/_tako/image?src=...&w=...`. Local public paths are available by default. Remote URLs must match the configured allowlist, and widths, qualities, and formats must match the configured guardrails. In JavaScript apps, use `imageUrl` for one optimized URL or `imageSrcSet` for plain `<img>` responsive sources.

## TLS And Routes

Routes live under `[envs.<env>]`:

```toml
[envs.production]
routes = [
  "example.com",
  "*.example.com/admin/*",
]
```

Tako issues certificates automatically:

- HTTP-01 for ordinary hostnames
- DNS-01 for wildcard routes after choosing DNS setup in `tako servers configure <name>`
- self-signed certs for local/private hostnames

If a wildcard route is deployed without DNS-01 provider config, deploy fails and tells you to run:

```bash
tako servers configure <name>
```

When HTTPS uses a non-default public port, deploy summaries include that port and HTTP redirects target it.

## Logs And Status

```bash
tako servers status
tako logs --env production
tako logs --env production --tail
tako releases list --env production
```

`tako servers status` uses signed HTTP remote management and can run from any directory. Logs include app stdout/stderr plus app-scoped Tako diagnostics.

## Server Operations

Reload without downtime:

```bash
tako servers reload la
```

Force a full service restart:

```bash
tako servers reload la --force
```

Upgrade one or all servers:

```bash
tako servers upgrade la
tako servers upgrade
```

Upgrade acquires a durable upgrade lock, installs the new binary, reloads through the service manager, waits for the management socket to report ready, and releases upgrade mode. If readiness fails, the previous on-disk binary is restored.

Remove a remote server install and its data:

```bash
tako servers uninstall la
```

Delete one app deployment target:

```bash
tako delete --env production --server la --yes
```

Deleting an app drains processes and removes that app's runtime data tree under `/opt/tako/apps/{app}/{env}`.
