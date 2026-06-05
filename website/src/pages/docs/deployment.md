---
layout: ../../layouts/DocsLayout.astro
title: "Self-Hosted Deployment - Tako Docs"
heading: Deployment
current: deployment
description: "Guide to deploying apps with Tako on your own servers, including server setup, rolling deploys, scaling, secrets, and production operations."
---

# Deployment

Tako deploys locally built apps to servers you control. The CLI validates configuration, builds and packages the release, uploads the artifact over signed private HTTP management, and asks `tako-server` to prepare and roll the release into traffic.

## Server Setup

Install `tako-server` on a Linux host:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

The installer creates the `tako` service user, the shared `tako-app` socket-access group, `/opt/tako`, `/var/run/tako`, service units with high file-descriptor limits, maintenance helpers, restricted sudoers policy, public HTTP/HTTPS listeners, local metrics, libvips runtime support, and private Tailscale management.

Custom public proxy ports:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo sh -s -- --http-port 8080 --https-port 8443
```

Service start requires a private Tailscale IP. The installer detects it with `tailscale ip -4` or uses `TAKO_MANAGEMENT_HOST`. For image builds or refreshes that should not touch the running service, set `TAKO_RESTART_SERVICE=0`.

## Add A Server

```bash
tako servers add prod-a.tailnet.ts.net --install
```

Add verifies Tailscale reachability, SSH recovery access as `tako@host`, server identity, signed HTTP management, and target metadata. It records the server in global `config.toml`, not in your project.

Then map your app environment:

```toml
[envs.production]
route = "app.example.com"
servers = ["prod-a"]
```

If `production` has no server list, deploy can offer to select one configured global server and write it to `tako.toml`.

## Deploy

```bash
tako deploy --env production
```

Deploy requires the environment to exist and define `route` or `routes`. `development` is reserved for `tako dev`.

Interactive production deploys ask for confirmation only when production is implicit. These are explicit and skip that confirmation:

```bash
tako deploy --env production
tako deploy --yes
```

## Preflight Validation

Before build work starts, deploy checks:

- target environment exists and is not `development`
- routes are valid
- selected servers exist and have `arch`/`libc` metadata
- required app secrets exist and are not expired
- selected S3 storage credentials exist and are not expired
- backup storage credentials exist and are not expired when backups are enabled
- provider credentials exist and are not expired when Cloudflare SSL or Let's Encrypt wildcard routes need them
- credentials expiring within 30 days are surfaced as warnings

Required Cloudflare credentials are also checked from each target server during remote prepare. Let's Encrypt wildcard routes verify zone read access from the server's egress IP.

## Build And Package

Deploy resolves runtime, package manager, preset, entrypoint, app root, assets, build stages, non-secret vars, release metadata, and runtime version.

Build stage precedence:

1. `[[build_stages]]`
2. `[build]`
3. runtime default build
4. no-op

Builds run in `.tako/build` after copying the source bundle root. Tako uses the git root when available, otherwise the selected config file's parent directory. It respects `.gitignore`, symlinks `node_modules/` from the original tree, and force-excludes `.git/`, `.tako/`, `.env*`, and `node_modules/` from deploy artifacts.

Version names are deterministic:

| Source state   | Version shape             |
| -------------- | ------------------------- |
| clean git tree | `<commit>`                |
| dirty git tree | `<commit>_<source-hash8>` |
| no git commit  | `nogit_<source-hash8>`    |

Target artifacts are cached locally and reused when inputs match. Invalid cache entries are rebuilt automatically.

## Upload And Prepare

Uploads go through private signed management:

```text
POST /release-artifact
```

The server verifies declared size and SHA-256, extracts into `/opt/tako/apps/{app}/{env}/releases/{version}/`, links release logs to the app log directory, prepares the per-app Unix identity and filesystem permissions, runs the runtime plugin's production install command as that app identity, and prepares runtime metadata.

Small management commands use `POST /rpc`. Logs use `POST /logs`. App/runtime management uses signed HTTP over Tailscale; SSH is for setup, recovery, reload, upgrade, and uninstall.

## Release Commands

Set a one-shot release command for migrations or cache preparation:

```toml
release = "bun run db:migrate"
```

The command runs once on the leader server, inside the new release directory as the app's per-app Unix identity, before rolling update. Followers wait for the leader result. If it fails, deploy aborts on every server, removes partial release directories, leaves `current` unchanged, and old instances keep serving.

Disable an inherited command for one environment:

```toml
[envs.staging]
release = ""
```

## Rolling Update

Each server rolls independently:

1. Start one new instance.
2. Wait for health.
3. Add it to the load balancer.
4. Drain one old instance.
5. Repeat until all desired instances are replaced.
6. Update `current`.
7. Prune releases older than 30 days.
8. Create a post-deploy backup when backups are enabled.

New app deploys start with desired instance count `1` per server. `tako scale` changes the desired count, and that value persists across restarts, deploys, and rollbacks. New deploys default to a maximum of two app instances per available CPU; explicit scale or deploy requests above the effective server maximum fail. If desired count is `0`, rolling deploy still starts one warm instance for the new build so traffic is immediately served after deploy.

## Scaling

```bash
tako scale 0 --env production
tako scale 2 --env production
```

Scaling to zero keeps routes registered. The first request after idle starts the app and waits for readiness. Cold-start coordination prevents a stampede of duplicate startups.

Scale can also target a specific server and app id:

```bash
tako scale 0 --server prod-a --app my-app/production
```

## HTTPS And Certificates

Public exact routes use Let's Encrypt HTTP-01 by default. Wildcard Let's Encrypt routes use Cloudflare DNS-01 and require:

```bash
tako credentials set ssl.cloudflare --env production
```

Cloudflare Origin CA is selected per environment:

```toml
[envs.production]
ssl = "cloudflare"
```

Cloudflare SSL also requires `ssl.cloudflare`. Direct browser connections to origins using Cloudflare Origin CA certificates will not trust those certificates.

Private/local route hostnames skip ACME and use self-signed certificates.

When HTTPS uses a non-default public port, deploy summaries include that port and HTTP redirects target it.

Tako only honors `X-Forwarded-Proto` and `Forwarded: proto=https` from loopback peers, Cloudflare peers, or peers listed in server `trusted_proxy.trusted_cidrs`. Direct clients that spoof those headers still receive normal HTTP-to-HTTPS redirects.

Deployed proxied app responses are compressed by `tako-server` when the client advertises Brotli or gzip and the response is safe to transform. Eligible text, JSON, JavaScript, CSS, XML, WASM, and SVG responses with `Content-Length >= 1024` get `Content-Encoding` plus `Vary: Accept-Encoding`; streaming, SSE, WebSocket/upgrade, already encoded, `no-transform`, small, and binary responses pass through unchanged.

## Source IPs

```toml
[envs.production]
source_ip = "auto"
```

| Value              | Behavior                                                                                                                                  |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------- |
| omitted or `auto`  | Use `CF-Connecting-IP` only for Cloudflare peers, then configured trusted proxy headers from trusted CIDRs, otherwise the direct peer IP. |
| `cloudflare-proxy` | Require a Cloudflare peer and valid `CF-Connecting-IP`; reject other requests with `403 Forbidden`.                                       |
| `trusted-proxy`    | Require loopback or a configured trusted proxy CIDR plus a valid forwarded client IP; reject invalid requests with `403 Forbidden`.       |
| `direct`           | Ignore proxy headers and use the TCP peer IP.                                                                                             |

Server-level trusted proxy config lives in `/opt/tako/config.json`:

```json
{
  "trusted_proxy": {
    "trusted_cidrs": ["127.0.0.1/32", "10.0.0.0/8"],
    "client_ip_headers": ["x-forwarded-for", "forwarded"]
  }
}
```

The same trusted-peer boundary controls forwarded HTTPS metadata used for redirects and upstream `X-Forwarded-Proto`.

## Secrets And Provider Credentials

Project secrets are encrypted in `.tako/secrets.json`:

```bash
tako secrets set DATABASE_URL --env production
```

Deploy compares the server's current secrets hash before sending secrets. Matching hashes skip secret transmission; stale or new servers receive the decrypted selected secrets over signed HTTP management. `tako-server` stores them encrypted in SQLite and injects them into fresh processes through fd 3.

Provider credentials use `tako credentials`, not `tako secrets`, and are not exposed to app code:

```bash
tako credentials set ssl.cloudflare --env production
```

## Storage

Storage bindings are declared in `tako.toml`:

```toml
[envs.production]
storages = { uploads = "prod_uploads" }

[storages.prod_uploads]
provider = "s3"
bucket = "app-uploads"
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"
public_base_url = "https://cdn.example.com/uploads"
```

S3 credentials are encrypted in `.tako/secrets.json` under the selected environment:

```bash
tako storages credentials prod_uploads --env production
```

Deploy sends runtime bindings over signed HTTP management and stores server-side storage bindings encrypted in SQLite. Local storage uses the built-in `local` resource and is rejected for multi-server deployed environments.

## Backups

Enable app data backups with a private S3-compatible resource:

```toml
[envs.production]
backup = { storage = "prod_backups" }

[storages.prod_backups]
provider = "s3"
bucket = "app-backups"
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"
```

Backup storage is not exposed to app code unless it is also listed in `[envs.<env>].storages`.

Backups include app-owned data and durable workflow state. Transient channel replay storage is excluded. Archives are compressed, encrypted, uploaded under `_tako/backups/{app}/{env}/{server}/`, and retained for 30 days by default.

Commands:

```bash
tako backups now --env production
tako backups list --env production
tako backups status --env production
tako backups download <backup-id> --env production --server prod-a
tako backups restore <backup-id> --env production --server prod-a --yes
```

## Releases And Rollbacks

```bash
tako releases list --env production
tako releases rollback <release-id> --env production
```

Release history is read from mapped servers and merged by release id. Rollback points the app/environment back to a previous release on each mapped server. Production rollback asks for confirmation unless `--yes` is passed.

## Operations

Reload server control plane without downtime:

```bash
tako servers reload prod-a
```

Force restart when recovery requires it:

```bash
tako servers reload prod-a --force
```

Upgrade server binaries:

```bash
tako servers upgrade
tako servers upgrade prod-a
```

Uninstall a server and remove all its data:

```bash
tako servers uninstall prod-a --yes
```

Delete one deployed target:

```bash
tako delete --env production --server prod-a --yes
```

## Metrics

`tako-server` exposes Prometheus metrics on localhost port `9898` by default:

```text
http://127.0.0.1:9898/metrics
```

Metrics cover requests, upstream latency, instance health, cold starts, deploys, TLS events, channel activity, workflow activity, image workers, and log drops. Use `--metrics-port 0` on `tako-server` to disable request/upstream metrics collection and the endpoint.

Request debug logs also include response compression algorithm, skip reason, and byte-count fields for diagnosing transfer behavior.
