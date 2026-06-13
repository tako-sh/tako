---
layout: ../../layouts/DocsLayout.astro
title: "Troubleshooting - Tako Docs"
heading: Troubleshooting
current: troubleshooting
description: "Troubleshoot common Tako problems including deploy failures, TLS issues, runtime errors, server status problems, and verbose diagnostics."
---

# Troubleshooting

Start with the local diagnostic report:

```bash
tako doctor
```

For remote state, use:

```bash
tako servers status
tako logs --env production
tako logs --env production --tail
```

Add `--verbose` for a timestamped execution transcript. Add `--ci` when you need deterministic output without prompts, colors, or spinners.

## Config And Init

### `tako.toml` already exists

Interactive `tako init` asks before overwriting. Non-interactive `tako init` leaves the file untouched and exits with an operation-cancelled result.

### Environment not found

Most app-scoped commands default to `production`. Make sure the environment exists:

```toml
[envs.production]
route = "app.example.com"
```

Use `--env <name>` when targeting a different environment.

### Development is reserved

`development` is reserved for `tako dev`. It cannot be deployed or deleted with `tako deploy` or `tako delete`.

### Removed v0-era shapes

Use the current config shape:

- Provider credentials are not in `tako.toml`. Use `tako credentials set ssl.cloudflare --env <env>` for Cloudflare certificates and `tako credentials set postgres_url --env <env>` for shared channel/workflow storage.
- Presets are not namespaced. Use `runtime = "bun"` and `preset = "tanstack-start"`.
- Storage resources are top-level `[storages.<name>]` tables plus `[envs.<env>].storages` bindings.
- The built-in `local` storage resource is not declared as `[storages.local]`.

## Local Development

### Local HTTPS does not work

Run:

```bash
tako doctor
```

On macOS, Tako checks the dev daemon, launchd dev proxy, boot helper, local DNS resolver files, loopback alias `127.77.0.1`, and TCP reachability on `127.77.0.1:443` and `:80`.

On Linux, Tako checks the dev daemon, systemd-resolved routing, iptables redirects, the loopback alias, and local CA trust. On NixOS, use the printed `configuration.nix` snippet.

### Browser shows certificate warnings

The local root CA should be installed into the system trust store on first run. Run `tako dev` again or `tako doctor` to trigger repair guidance. The public root certificate is at:

```text
{TAKO_HOME}/ca/ca.crt
```

Some tools also need:

```bash
export NODE_EXTRA_CA_CERTS="{TAKO_HOME}/ca/ca.crt"
```

### `.test` does not resolve

Tako manages `.test` and `.tako.test`. On macOS, `/etc/resolver/test` and `/etc/resolver/tako.test` point to the local DNS listener. If `/etc/resolver/test` already exists and was not created by Tako, Tako skips it and `.tako.test` remains the fallback.

Use explicit external development routes for tunnels or custom hostnames:

```toml
[envs.development]
routes = ["my-tunnel.example.com"]
```

External routes are proxied by Tako but are not rewritten to `.local`, advertised by mDNS, or resolved by Tako DNS.

### Unknown local host returns 421

Unknown managed `.test` and `.tako.test` hosts return a helpful `421 Misdirected Request` with registered dev routes. Unknown `.local` and external hosts return a generic 421.

### Dev app does not restart after changes

Tako restarts when effective dev vars, secrets, storage bindings, channel definitions, workflow definitions, or generated declaration files change. Source hot reload is handled by your runtime or framework dev command.

If a workflow import crashes before claiming work, dev marks the worker unhealthy briefly and enqueue calls fail loudly instead of hanging. Fix the import error and enqueue again.

## Server Setup

### `tako servers add` cannot reach the server

The host should be a Tailscale MagicDNS name or Tailscale IP. Tako verifies Tailscale reachability, SSH recovery access as `tako@host`, server identity, and signed HTTP management before writing global `config.toml`.

If SSH host keys are unknown or changed, fix `~/.ssh/known_hosts`; Tako does not bypass host-key verification.

### Install fails because Tailscale is missing

Normal server installs require a private Tailscale management address. Install fails with a message that remote management requires Tailscale. Start Tailscale on the server or set `TAKO_MANAGEMENT_HOST` when you know the correct private address.

### Server target metadata is missing

Deploy requires each selected server to have `arch` and `libc` target metadata. Re-add the server with SSH checks enabled:

```bash
tako servers remove prod-a
tako servers add prod-a.tailnet.ts.net
```

### Server is unhealthy after reload or upgrade

Check:

```bash
tako servers status
tako servers reload prod-a
tako servers reload prod-a --force
tako servers upgrade prod-a
```

`reload` is zero-downtime by default. `--force` performs a full service restart and may briefly interrupt apps.

If `tako servers upgrade` uses a custom `TAKO_DOWNLOAD_BASE_URL`, signature verification for that custom checksum manifest is skipped, but the archive SHA-256 is still verified after download. Non-HTTPS custom bases are rejected unless `TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1` is explicitly set for local testing.

## Deploy Failures

### Production deploy asks for confirmation

Deploying to production with an implicit environment asks for confirmation in interactive terminals. Use one of:

```bash
tako deploy --env production
tako deploy --yes
```

### No servers are configured

In an interactive terminal, deploy, logs, and secret sync can offer the add-server wizard. In CI, configure servers first:

```bash
tako servers add prod-a.tailnet.ts.net --install
```

Then map the environment:

```toml
[envs.production]
servers = ["prod-a"]
```

### Build output is missing

Deploy verifies the resolved `main` exists in the built app directory. If the preset default does not match your project, set `main` explicitly:

```toml
main = "dist/server/entry.mjs"
```

Use `[build]` or `[[build_stages]]` to produce the files Tako should package.

### Build stages conflict

`[build].run` and `[[build_stages]]` are mutually exclusive. `[build].include` and `[build].exclude` cannot be used with `[[build_stages]]`; use per-stage `exclude`.

### Deploy reports a stale or missing runtime

Pin the runtime when you need deterministic server-side runtime resolution:

```toml
runtime = "bun@1.2.3"
```

### Container deploy cannot build or start

Container releases require Docker or Podman on the target server. The container must listen on `$PORT` (`3000` today), bind `0.0.0.0`, and use the Tako SDK so `/status` echoes the internal health-probe token.

Container releases are HTTP-only in v0. Use native releases when the app needs workflows, the internal socket, or `TAKO_DATA_DIR`.

Without a pin, deploy runs the local runtime's `--version` and falls back to `latest`.

### Deploy fails during release command

The `release` command runs once on the leader server as the app's per-app Unix identity before rolling update. If it exits non-zero, times out, or is signaled, deploy aborts everywhere, removes partial release directories, leaves `current` untouched, and old instances keep serving.

Disable an inherited release command for one environment:

```toml
[envs.staging]
release = ""
```

### Another deploy is already running

Each server has a per-app deploy lock. Wait for the current deploy to finish and retry. Restarting `tako-server` clears the in-memory lock, but the interrupted deploy itself fails and should be retried.

## Secrets And Credentials

### Secret is missing or expired

Set or rotate it:

```bash
tako secrets set DATABASE_URL --env production --expires-on "in 90 days"
tako deploy --env production
```

Deploy fails before build work when selected app secrets are expired, and warns when selected secrets expire within 30 days.

### Secrets changed but app still sees old values

Use:

```bash
tako secrets sync --env production
```

Secret sync updates encrypted server state, restarts workflow workers, and rolls HTTP instances so fresh processes receive the new fd-3 bootstrap data.

### Another machine cannot decrypt secrets

Export and import the environment key:

```bash
tako secrets key export --env production
tako secrets key import --env production
```

Teams that prefer a memorized shared secret can initialize a key with:

```bash
tako secrets key import --passphrase --env production
```

### Wildcard TLS credentials are missing

Let's Encrypt wildcard routes require Cloudflare DNS-01 credentials:

```bash
tako credentials set ssl.cloudflare --env production --expires-on "in 90 days"
```

Cloudflare SSL also requires `ssl.cloudflare`. Multi-server channel deploys require `postgres_url`; multi-server JS workflow deploys require `postgres_url` unless every workflow uses `local: true` in its `defineWorkflow(...)` option object. Multi-server Go workflow deploys require `postgres_url`. Provider credentials are encrypted under the environment's `credentials` object and are not exposed to app code or `tako secrets sync`.

Deploy verifies required Cloudflare credentials from each target server during remote prepare. For Let's Encrypt wildcard routes, use a Cloudflare user or account API token with Zone Read and DNS Write for the matching Cloudflare zone, and include each target server's egress IP in any token IP restriction.

## Storage And Backups

### S3 storage credentials are missing or expired

Set or rotate them:

```bash
tako storages add uploads \
  --env production \
  --resource prod_uploads \
  --provider s3 \
  --bucket app-uploads \
  --endpoint https://<account>.r2.cloudflarestorage.com \
  --region auto

tako storages credentials prod_uploads --env production
```

Deploy fails early for selected expired S3 credentials and warns for credentials expiring within 30 days.

### Local storage fails in multi-server deploys

The built-in `local` resource can deploy only to single-server environments. Use an S3-compatible resource for multi-server deploys.

### Backup setup fails

Backups need a declared private S3-compatible resource:

```toml
[envs.production]
backup = { storage = "prod_backups" }

[storages.prod_backups]
provider = "s3"
bucket = "app-backups"
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"
```

Do not set `public_base_url` on backup storage, and do not use `local` for backups. Backup keys are created automatically by deploy or `tako backups now` when needed.

### Restore did not affect every server

Backups are per server. List backups and restore the server you want:

```bash
tako backups list --env production
tako backups restore <backup-id> --env production --server prod-a --yes
```

Backup objects live under `_tako/backups/{app}/{env}/{server}/`.

## Source IP And Redirects

### Source IP is wrong behind a proxy

Default `source_ip = "auto"` uses Cloudflare headers only for Cloudflare peers, configured trusted proxy headers only for trusted CIDRs, then the direct peer IP.

For Cloudflare-only traffic:

```toml
[envs.production]
source_ip = "cloudflare-proxy"
```

For nginx, HAProxy, Caddy, Traefik, or another front proxy:

```toml
[envs.production]
source_ip = "trusted-proxy"
```

Then configure server-level `trusted_proxy.trusted_cidrs` in `/opt/tako/config.json` for non-loopback proxies.

### HTTP redirect loop behind a TLS-terminating proxy

Make sure the immediate peer IP is loopback, a Cloudflare IP, or listed in `trusted_proxy.trusted_cidrs`. Tako ignores `X-Forwarded-Proto` and `Forwarded: proto=https` from untrusted direct clients.

Use `source_ip = "direct"` when you want to ignore all proxy headers.

### Response is not compressed

Deployed app responses are compressed only when the browser sends `Accept-Encoding` with Brotli or gzip and the upstream response is transformable. Tako skips streaming responses, SSE, WebSockets/upgrades, `Cache-Control: no-transform`, existing `Content-Encoding`, responses below 1024 bytes, unknown-length bodies, and unsupported binary content types. Use verbose server logs to inspect the response compression algorithm or skip reason.

## Images

### Public image request is rejected

Check the query and allowlists:

- `src` and `w` are required.
- `w` must be in `[images].sizes`.
- `q`, when present, must be in `[images].qualities`.
- `f`, when present, must be in `[images].formats`.
- Remote sources must match `[images].remote_patterns`.
- Local sources must match `[images].local_patterns`, which defaults to `["/**"]`.

Remote image sources reject unsupported schemes, userinfo, fragments, recursive optimizer URLs, private/local hosts and IPs, private/local DNS results, and redirects.

### Private storage image helper fails

Private storage image transforms are not available yet. Use `createDownloadUrl` for private object access. Public storage image helpers require an S3 binding with `public_base_url` and `{ public: true }`.

## Logs And Diagnostics

### Logs look empty

Confirm the environment and servers:

```bash
tako logs --env production --days 7
tako logs --env production --tail
```

History mode reads bounded bytes from `previous.log` and `current.log`, filters by timestamp, sorts across servers, and pages output when stdout is interactive. Streaming mode polls the same endpoint with offsets.

### Need machine-readable logs

Use JSONL:

```bash
tako logs --env production --json
```

Structured app and worker JSON records are preserved and annotated with `source`, `instance_id`, and `server` when multiple servers are involved.

### Request is slow or fails through the proxy

Run `tako logs --env production --tail` while reproducing the request. Proxy diagnostics appear with source `proxy` and include `request_id`, app, instance, route, handler/cache result, status, total latency, cold-start wait time, and upstream response-header latency when the request reaches an app instance.

Pass your own `X-Request-ID` header when reproducing a request, or use the generated `request_id` from the proxy log line to correlate proxy diagnostics with app logs.
