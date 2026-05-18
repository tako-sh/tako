---
layout: ../../layouts/DocsLayout.astro
title: "Troubleshooting deploy failures, TLS issues, and runtime errors - Tako Docs"
heading: Troubleshooting
current: troubleshooting
description: "Troubleshoot common Tako problems including deploy failures, TLS issues, runtime errors, server status problems, and verbose diagnostics."
---

# Troubleshooting

Start with a local snapshot:

```bash
tako doctor
```

Then rerun the failing command with verbose output:

```bash
tako -v deploy --env production
```

For automation, add `--ci` to disable interactive prompts and pretty UI:

```bash
tako --ci deploy --env production --yes
```

Status, progress, prompts, and logs go to stderr. Machine-readable command results go to stdout.

## Config Not Found

Commands that need an app config look for `./tako.toml` by default:

```text
tako.toml not found
```

Run the command from the app directory or pass an explicit config:

```bash
tako -c apps/web/tako.toml deploy --env production
```

If the path passed to `--config` has no `.toml` suffix, Tako appends it.

## Invalid App Or Server Names

App names, server names, environment names, and workflow worker group names are intentionally strict. Use lowercase letters, numbers, and hyphens; start with a lowercase letter; do not end with a hyphen; stay under 64 characters.

Storage binding and resource names can also use underscores.

Fix the value in `tako.toml`, then rerun the command.

## Unknown `tako.toml` Key

Tako rejects unknown top-level keys and unknown nested keys in strict sections:

```text
Unknown key '...'
```

Common causes:

- Putting DNS provider settings in `tako.toml`. Use `tako dns configure --env <env>` instead.
- Putting global server inventory under app `[servers]`. Use `tako servers add`; app `[servers.<name>]` is only for per-app workflow overrides.
- Using a namespaced preset in `tako.toml`, such as `preset = "js/tanstack-start"`. Set `runtime = "bun"` and `preset = "tanstack-start"`.

## Environment Not Found

Deploy, logs, releases, and app-scoped secret sync require the selected environment to exist in `tako.toml`:

```toml
[envs.production]
route = "app.example.com"
servers = ["prod-a"]
```

`--env` defaults to `production`. Add the environment or pass the intended one:

```bash
tako deploy --env staging
```

`development` is reserved for `tako dev` and cannot be deployed.

## No Servers Configured

Deploy, logs, releases, and scale need target servers. Add a server first:

```bash
tako servers add prod-a.tailnet.ts.net
```

Then reference it from the environment:

```toml
[envs.production]
route = "app.example.com"
servers = ["prod-a"]
```

If `production` has no server mapping and exactly one global server exists, interactive deploy can write that server into `[envs.production].servers`.

## Server Add Fails

`tako servers add` verifies:

- The host is a Tailscale MagicDNS name or Tailscale IP.
- `tako@host` SSH recovery access works.
- Signed remote management works over the private Tailscale HTTP endpoint.
- Server target metadata such as architecture and libc can be detected.
- Public HTTP and HTTPS ports are detected.

If verification fails, the server is not written to global `config.toml`.

When adding a fresh host, use install mode:

```bash
tako servers add ubuntu@prod-a.tailnet.ts.net
```

or:

```bash
tako servers add prod-a.tailnet.ts.net --install --admin-user ubuntu
```

Passing `admin-user@host` is shorthand for using that admin user and running install or repair when needed.

## Remote Management Requires Tailscale

Normal server installs bind management RPC to the server's Tailscale address on port `9844`. If install cannot find a Tailscale IP, it fails with a message explaining that remote management requires Tailscale.

Fix Tailscale on the server, or pass `TAKO_MANAGEMENT_HOST` to the server installer when you know the correct private address.

## Deploy Lock Already Held

Non-dry-run deploys acquire a project-local lock at `.tako/deploy.lock`.

If another deploy is running, the later command exits immediately with the owning PID. Wait for the active deploy to finish. If the process crashed, rerun deploy; stale lock handling does not require manual cleanup in normal cases.

## Secrets Missing Or Expired

Set app secrets per environment:

```bash
tako secrets set DATABASE_URL --env production
```

Interactive `set` prompts for the value and optional expiry. Non-interactive `set` reads one line from stdin:

```bash
printf '%s\n' "$DATABASE_URL" | tako secrets set DATABASE_URL --env production --expires-on "in 90 days"
```

Deploy fails before build work starts if any selected environment secret is expired. It warns when a secret expires within 30 days.

If a teammate cannot decrypt secrets, import the environment key:

```bash
tako secrets key import --env production
```

or use passphrase mode:

```bash
tako secrets key import --passphrase --env production
```

## Storage Credentials Missing Or Expired

Attach S3-compatible storage with:

```bash
tako storages add uploads \
  --env production \
  --provider s3 \
  --bucket my-app-prod \
  --endpoint https://example.r2.cloudflarestorage.com \
  --region auto
```

The command writes binding metadata to `tako.toml` and encrypted credentials to `.tako/secrets.json`.

Deploy fails early if selected S3 credentials are missing or expired, warns if they expire within 30 days, and checks that credentials do not exist for unbound resources.

For local storage, use:

```bash
tako storages add uploads --env production --provider local
```

Local storage uses the built-in `local` resource and writes `storages = { uploads = "local" }`. It has no `[storages.local]` table, configurable path, or user-provided credentials.

## Wildcard Routes Need DNS

Wildcard production routes require DNS-01 credentials:

```toml
[envs.production]
routes = ["app.example.com", "*.app.example.com"]
```

Configure Cloudflare DNS for that app environment:

```bash
tako dns configure --env production --expires-on "in 90 days"
```

The token must be able to read zones and edit DNS records for the zone. It is encrypted in `.tako/secrets.json`, not stored in `tako.toml`.

Deploy fails early if wildcard routes need DNS credentials and none are configured, or if the configured token has expired. It warns when the token expires within 30 days.

## Cloudflare Or Proxy Source IP Problems

Generated configs use `source_ip = "auto"` implicitly. Auto mode uses:

1. `CF-Connecting-IP` when the peer is a Cloudflare IP.
2. Configured trusted proxy headers when the peer is trusted.
3. The direct peer IP.

Strict Cloudflare mode rejects anything that is not a valid Cloudflare request:

```toml
[envs.production]
source_ip = "cloudflare-proxy"
```

Use this when your app should only receive public traffic through Cloudflare. Non-Cloudflare requests return `403 Forbidden`.

For nginx, HAProxy, Caddy, Traefik, or another front proxy:

```toml
[envs.production]
source_ip = "trusted-proxy"
```

Then configure server-level `trusted_proxy.trusted_cidrs` in `/opt/tako/config.json` for non-loopback proxies. Without a trusted peer and valid forwarded header, strict trusted-proxy mode returns `403 Forbidden`.

Use direct mode to ignore proxy headers:

```toml
[envs.production]
source_ip = "direct"
```

## Cloudflare IP Ranges Seem Stale

`tako-server` starts with bundled Cloudflare IP ranges, overlays a valid disk cache from the server data directory, and refreshes every 24 hours while any active route uses `auto` or `cloudflare-proxy`.

If the API refresh fails, the server keeps the current in-memory list and logs a warning. Restarting the server reloads the bundled list and any last-known-good cache.

## Build Fails

Check the resolved runtime and preset:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Then check build configuration:

```toml
[build]
run = "bun run build"
```

`[build].run` and `[[build_stages]]` are mutually exclusive. `[build].include` and `[build].exclude` cannot be used with `[[build_stages]]`; use per-stage `exclude` instead.

Deploy bundles source from the git root when available, otherwise from the app directory. It always excludes `.git/`, `.tako/`, `.env*`, and `node_modules/`.

## Entrypoint Not Found

Tako resolves the deploy entrypoint from:

1. Top-level `main` in `tako.toml`.
2. Manifest main, such as `package.json` `main`.
3. Preset `main`.

Set `main` when the automatic choice is wrong:

```toml
main = "dist/server/tako-entry.mjs"
```

For JS presets pointing to `index.<ext>` or `src/index.<ext>`, Tako searches common root and `src/` entrypoint files before using the preset fallback.

## Release Command Fails

Release commands run once on the leader server before rolling update:

```toml
release = "bun run db:migrate"
```

The command runs as `sh -c` in the release directory after production dependencies are installed. It receives app vars and freshly decrypted app secrets.

If it exits non-zero or times out, deploy aborts on every server, removes the partial release through signed management, leaves `current` unchanged, and old instances keep serving. Check the stderr tail in deploy output and the app logs:

```bash
tako logs --env production --days 1
```

## Runtime Download Fails

`tako-server` downloads Bun and Node runtimes when needed, verifies checksum files, and installs them under the server data directory. Go deploys a compiled binary and does not need a server-side runtime download.

Common causes:

- The server cannot reach GitHub or nodejs.org.
- The runtime version is invalid.
- The runtime archive or checksum download exceeds the configured safety limits.
- The checksum does not match.

Pin the runtime in `tako.toml` when you need a specific runtime version:

```toml
runtime = "bun@1.2.3"
```

## App Starts But Requests Return 502 Or 504

The app process must bind to `127.0.0.1` on an OS-assigned port and report the bound port on fd 4. The SDK entrypoints handle this automatically.

For JavaScript apps, use the `tako.sh` runtime entrypoint or a framework preset. Direct Vite dev commands need the `tako.sh/vite` plugin for fd-4 readiness.

For Go apps, use:

```go
tako.ListenAndServe(handler)
```

If startup fails during deploy, deploy fails. If a later cold start fails, users receive generic `502` or `504` responses while details go to app logs.

## Logs Are Empty

Fetch recent logs:

```bash
tako logs --env production --days 3
```

Stream live logs:

```bash
tako logs --env production --tail
```

Logs use signed HTTP management. If no logs are found, make sure the environment exists, the target server is mapped, signed management works, and the app has been deployed to that environment.

Use JSON output for tooling:

```bash
tako logs --env production --json
```

## Status Cannot Reach Servers

`tako servers status` uses signed HTTP remote management over Tailscale. It does not require `tako.toml` and can run from any directory.

If status fails:

- Confirm Tailscale is running locally and on the server.
- Confirm the server still has the enrolled management key.
- Run `tako servers reload <name>` or `tako servers upgrade <name>` if the service is unhealthy.
- Re-add the server if target metadata or public ports are stale.

## TLS Or Certificate Issues

Exact public hostnames use HTTP-01 challenges. Wildcard hostnames use Cloudflare DNS-01. Local/private hostnames use self-signed certificates.

For public exact routes, make sure ports 80 and 443 reach `tako-server`.

For wildcard routes, make sure DNS credentials are configured for the app environment:

```bash
tako dns configure --env production
```

For development TLS, rerun:

```bash
tako doctor
tako dev
```

On macOS, `tako dev` sets up the local CA, loopback proxy, and DNS resolver. On Linux, it configures loopback and local DNS/proxy helpers.

## Public Images Fail

The image optimizer fails closed. Public requests require:

- `src`
- `w`
- optional `q`
- optional `f`

The width must be in `[images].sizes`, quality must be in `[images].qualities`, and format must be in `[images].formats`.

Remote images must match `[images].remote_patterns`. Local sources must match `[images].local_patterns`, which defaults to `["/**"]` unless overridden.

Storage image URLs also require the storage binding to be configured and current. Public storage URLs require `public_base_url`.

## Delete Is Ambiguous

`tako delete` deletes one deployed app/environment/server target. In non-interactive mode, pass enough flags:

```bash
tako delete --env production --server prod-a --yes
```

Outside a project directory, run interactively to choose a target or pass `--server` and enough context for discovery.

## Scale Needs A Target

From a project directory:

```bash
tako scale 0 --env production
```

Outside a project directory, pass the deployed app id:

```bash
tako scale 0 --server prod-a --app my-app/production
```

Scale settings are per targeted server and persist across restarts, deploys, and rollbacks.

## Safe Recovery Cases

| Problem                              | Recovery                                                                 |
| ------------------------------------ | ------------------------------------------------------------------------ |
| Config/data directory deleted        | Recreated on the next command.                                           |
| `.tako/` deleted                     | Recreated on next deploy or secret/storage/DNS write.                    |
| `tako.toml` deleted                  | Commands that need project config fail with guidance to run `tako init`. |
| `tako-server` restarts during deploy | The deploy fails; rerun it after the server is healthy.                  |
| Network interruption during deploy   | Retry after checking server status.                                      |
| Low free space under `/opt/tako`     | Deploy fails before upload with required vs available disk sizes.        |
