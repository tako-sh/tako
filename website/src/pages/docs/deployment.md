---
layout: ../../layouts/DocsLayout.astro
title: "Self-hosted app deployment: server setup, rolling deploys, and scaling - Tako Docs"
heading: Deployment
current: deployment
description: "Guide to deploying apps with Tako on your own servers, including server setup, rolling deploys, scaling, secrets, and production operations."
---

# Deployment

Tako deploys locally built apps to servers you control. The CLI builds the release, validates app configuration, sends the artifact over the signed management path, and asks `tako-server` to prepare and roll the new release into traffic.

## Server Setup

Install `tako-server` on each host:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

The installer creates the `tako` service user, the `tako-app` runtime user, `/opt/tako`, `/var/run/tako`, service-manager units, maintenance helpers, private Tailscale remote management, and the default public HTTP/HTTPS listeners. The server starts after install when Tailscale is available.

Register the host locally:

```bash
tako servers add host.example.com --name la
```

If the host is not installed yet or needs repair:

```bash
tako servers add ubuntu@host.example.com --install --name la
```

`tako servers add` records server metadata and listener ports. It does not ask for app routes, DNS, storage, or source-IP settings; those are app-environment settings and are applied during deploy.

Custom public ports are set at install time:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo sh -s -- --http-port 8080 --https-port 8443
```

Service start requires a private Tailscale IP for remote management. For image builds or refreshes that should not touch the running service, set `TAKO_RESTART_SERVICE=0`.

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

Deploy requires valid metadata for every selected server. Server names use the same shape as app names: lowercase letters, numbers, hyphens, start with a lowercase letter, end with a letter or number, and at most 63 characters.

## App Environment

Map a deployable environment to route patterns and server names in `tako.toml`:

```toml
name = "dashboard"

[envs.production]
routes = ["dashboard.example.com", "*.dashboard.example.com"]
servers = ["la", "nyc"]
source_ip = "cloudflare-proxy"
idle_timeout = 300

[envs.staging]
route = "staging.example.com"
servers = ["staging"]
source_ip = "direct"
```

The remote deployment identity is `{app}/{env}`. The same physical server can host multiple environments for the same app because each environment gets a separate identity and filesystem path.

`development` is reserved for `tako dev` and cannot be deployed.

## Deploy Flow

Run:

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

`--env` defaults to `production`. Interactive production deploys ask for confirmation only when the environment is implicit. Passing `--env production` or `--yes` makes the target explicit.

Before build or upload, deploy validates:

- the selected environment exists
- routes are present
- all server names exist and have target metadata
- required app secrets can be decrypted
- storage credentials can be decrypted when configured
- DNS credentials exist when wildcard routes require DNS-01
- expiring or expired credentials are surfaced before work starts

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

Target artifacts are cached under `.tako/artifacts/`. Cache entries are verified by checksum and size before reuse, and invalid entries are rebuilt.

## Runtime Metadata

Each release contains `app.json`, the runtime manifest used by `tako-server`. It includes the resolved runtime, entrypoint, package manager, runtime version, non-secret environment variables, JS app root, image optimizer config, idle timeout, and release metadata.

If `runtime_version` is set in `tako.toml`, deploy uses it directly. Otherwise, deploy runs `<runtime> --version` and falls back to `latest`.

After extraction, `tako-server` runs the runtime plugin's production install command. App build steps run locally, not on the server.

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
- secrets and storage bindings are injected for that run
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

If startup fails during a warm deploy, deploy fails. If a later cold start fails, users receive generic `502` or `504` responses while details go to app logs.

## Routes And TLS

Routes live under `[envs.<env>]`:

```toml
[envs.production]
routes = [
  "example.com",
  "*.example.com/admin/*",
]
```

Tako issues certificates automatically:

- HTTP-01 for ordinary public hostnames
- Cloudflare DNS-01 for wildcard routes
- self-signed certs for local or private hostnames

Cloudflare DNS credentials are configured separately from `tako.toml`:

```bash
tako dns configure --env production --expires-on "in 90 days"
```

The token must be able to read zones and edit DNS records for the zone. Tako encrypts it in `.tako/secrets.json` under the selected environment's DNS credentials. Deploy fails early when wildcard routes need a missing or expired token, and warns when the token expires within 30 days.

When HTTPS uses a non-default public port, deploy summaries include that port and HTTP redirects target it.

## Source IPs

`source_ip` controls how the proxy decides the original client IP for an app environment:

| Value              | Behavior                                                                                                                                                               |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| omitted or `auto`  | Use `CF-Connecting-IP` only for requests from Cloudflare IP ranges, then explicitly configured trusted proxy headers from trusted CIDRs, otherwise the direct peer IP. |
| `cloudflare-proxy` | Require a Cloudflare peer and `CF-Connecting-IP`; reject non-Cloudflare requests with `403 Forbidden`.                                                                 |
| `trusted-proxy`    | Require loopback or a configured trusted proxy CIDR plus a valid forwarded client IP; reject invalid requests with `403 Forbidden`.                                    |
| `direct`           | Ignore proxy headers and use the TCP peer IP.                                                                                                                          |

Use `cloudflare-proxy` when the route is intentionally behind Cloudflare. It is stricter than the default and protects the app if the firewall accidentally allows direct public traffic.

Use `trusted-proxy` for nginx, HAProxy, or a load balancer in front of Tako. Server-level trusted proxy settings live in `/opt/tako/config.json`, not `tako.toml`:

```json
{
  "trusted_proxy": {
    "trusted_cidrs": ["127.0.0.1/32", "10.0.0.0/8"],
    "client_ip_headers": ["x-forwarded-for", "forwarded"]
  }
}
```

Supported headers are `cf-connecting-ip`, `x-forwarded-for`, and `forwarded`. `proxy_protocol` also requires `trusted_cidrs`.

Tako ships with bundled Cloudflare IP ranges, loads a last-known-good disk cache when available, keeps ranges in memory, and refreshes them daily while running when any app route uses `auto` or `cloudflare-proxy`.

## Secrets

Local app secrets are encrypted in `.tako/secrets.json`. Keys live outside the repo and can be exported or imported:

```bash
tako secrets set DATABASE_URL --env production --expires-on "in 90 days" --sync
tako secrets key export --env production
tako secrets key import --env production
```

Each secret entry stores an encrypted value and optional plaintext `expires_on` date metadata. Use `YYYY-MM-DD` or `in N days` when expiry is known. Use `--expires-on never` or omit the flag when it is unknown.

Deploy compares a server secrets hash before sending secrets. If hashes match, secrets are omitted and the server keeps its current encrypted values. If they differ, deploy sends decrypted secrets over the signed management path and `tako-server` stores them encrypted in SQLite.

Fresh HTTP instances and workflow workers receive secrets through fd 3. Secret syncs trigger worker restart and HTTP rolling restart.

## Storage

Attach object storage before deploy:

```bash
tako storages add uploads \
  --env production \
  --resource prod_uploads \
  --provider s3 \
  --bucket app-uploads \
  --endpoint https://<account>.r2.cloudflarestorage.com \
  --region auto \
  --expires-on "in 90 days"
```

Storage bindings and non-secret S3 provider metadata live in `tako.toml`. S3 credentials are encrypted in `.tako/secrets.json` under the selected environment's `storages` map with optional `expires_on` metadata. R2 uses `provider = "s3"` with the R2 S3-compatible endpoint. Local storage uses the built-in `local` resource name and does not declare `[storages.local]`.

Deploy fails early if selected S3 credentials are expired, warns if they expire within 30 days, rejects local storage on multi-server environments, sends runtime bindings over the signed management path, and stores server-side bindings encrypted in SQLite.

Fresh HTTP instances and workflow workers receive storage bindings through fd 3 alongside secrets.

## Images

Public optimized images are configured in `tako.toml`:

```toml
[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
```

The optimizer endpoint is `/_tako/image?src=...&w=...`. Local public paths are available by default. Remote URLs must match the configured allowlist, and widths, qualities, and formats must match the configured guardrails.

In JavaScript apps, use `imageUrl` for one optimized URL or `imageSrcSet` for plain `<img>` responsive sources.

## Scaling

Desired instance count is stored on each server:

```bash
tako scale 2 --env production
tako scale 0 --env production
```

`N > 0` keeps at least `N` healthy instances running. `0` enables scale-to-zero: deploy keeps one warm instance initially, then idle instances stop after `idle_timeout`. The next request wakes the app and waits for readiness.

Scale settings survive server restarts, deploys, and rollbacks.

## Logs And Releases

```bash
tako logs --env production
tako logs --env production --tail
tako releases list --env production
tako releases rollback <release-id> --env production
```

Logs include app stdout/stderr plus app-scoped Tako diagnostics. Rollback uses the same rolling update path as deploy.

## Server Operations

Check all servers from any directory:

```bash
tako servers status
```

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
