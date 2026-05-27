---
layout: ../../layouts/DocsLayout.astro
title: "tako.toml Reference - Tako Docs"
heading: "tako.toml Reference"
current: tako-toml
description: "Complete tako.toml reference covering routes, runtime settings, builds, secrets, scaling, environments, and deployment configuration."
---

# `tako.toml` Reference

`tako.toml` describes one app: its identity, runtime, build, routes, environment variables, storage bindings, backups, SSL provider, source-IP policy, workflows, and target servers. Secrets and credentials do not live in this file; they are encrypted in `.tako/secrets.json`.

## Minimal Config

```toml
name = "my-app"
runtime = "bun"
preset = "tanstack-start"

[envs.production]
route = "my-app.example.com"
servers = ["prod-a"]
```

Generated configs stay small. Defaults such as `source_ip = "auto"`, `idle_timeout = 300`, `ssl = "letsencrypt"`, and `app_root = "src"` are omitted unless you set them.

## Full Example

```toml
name = "dashboard"
runtime = "bun@1.2.3"
package_manager = "bun"
preset = "tanstack-start"
app_root = "src"
assets = ["public"]
release = "bun run db:migrate"

[build]
run = "bun run build"
install = "bun install"
cwd = "."
include = ["dist/**", "package.json"]
exclude = ["**/*.map"]

[vars]
LOG_LEVEL = "info"

[vars.production]
LOG_LEVEL = "warn"

[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
sizes = [320, 640, 960, 1200, 1920]
qualities = [75]
formats = ["webp", "avif"]

[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1

[envs.production]
route = "dashboard.example.com"
servers = ["prod-a"]
source_ip = "cloudflare-proxy"
ssl = "cloudflare"
idle_timeout = 300
storages = { uploads = "prod_uploads" }
backup = { storage = "prod_backups" }

[storages.prod_uploads]
provider = "s3"
bucket = "dashboard-uploads"
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"
public_base_url = "https://cdn.example.com/uploads"

[storages.prod_backups]
provider = "s3"
bucket = "dashboard-backups"
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"

[servers.prod-a.workflows.email]
workers = 2
```

## Top-Level Keys

| Key                     | Type   | Meaning                                                                                                                          |
| ----------------------- | ------ | -------------------------------------------------------------------------------------------------------------------------------- |
| `name`                  | string | Required app name. Used for deploy ids, data paths, default dev hostnames, and generated examples.                               |
| `runtime`               | string | Runtime id, optionally pinned as `<id>@<version>`, for example `bun@1.2.3`, `node@22.0.0`, or `go`.                              |
| `package_manager`       | string | JS package-manager override: `bun`, `npm`, `pnpm`, or `yarn`, optionally with a version suffix.                                  |
| `preset`                | string | Framework preset alias such as `vite`, `tanstack-start`, or `nextjs`.                                                            |
| `app_root`              | string | JS app root for `channels/`, `workflows/`, and preferred `tako.d.ts` placement. Defaults to `src`. Use `.` for root-level files. |
| `main`                  | string | Runtime entrypoint override, relative to the app directory.                                                                      |
| `dev`                   | array  | Custom dev command for `tako dev`.                                                                                               |
| `assets`                | array  | Static asset roots copied into deployed `public/`, merged after build.                                                           |
| `release`               | string | One-shot command run once on the leader server before rolling update.                                                            |
| `[build]`               | table  | Single-stage deploy build settings.                                                                                              |
| `[[build_stages]]`      | array  | Multi-stage deploy build settings. Mutually exclusive with `[build].run`.                                                        |
| `[vars]`                | table  | Non-secret vars shared by all environments.                                                                                      |
| `[vars.<env>]`          | table  | Non-secret vars for one environment.                                                                                             |
| `[envs.<env>]`          | table  | Routes, servers, storage, backup, SSL, source-IP, and scaling policy for one environment.                                        |
| `[storages.<resource>]` | table  | Non-secret S3-compatible storage metadata.                                                                                       |
| `[images]`              | table  | Public image optimizer allowlists and output settings.                                                                           |
| `[workflows]`           | table  | Workflow worker defaults.                                                                                                        |
| `[workflows.<group>]`   | table  | Named workflow worker group overrides.                                                                                           |
| `[servers.<name>]`      | table  | Per-app per-server overrides. Currently supports workflow settings.                                                              |

## Runtime And Preset

Runtime selection controls the base adapter and default runtime plugin. Preset selection adds framework defaults.

```toml
runtime = "bun"
preset = "vite"
```

JavaScript runtimes detect package managers from `package.json` `packageManager`, then lockfiles, unless `package_manager` is set. Go uses `go` and has no production dependency install.

Runtime version pins are written as part of `runtime`:

```toml
runtime = "node@22.3.0"
```

Deploy uses the pin when present. Otherwise it runs the local runtime's `--version` and falls back to `latest`.

## Entrypoints And Assets

`main` overrides the runtime entrypoint. If omitted, Tako resolves from package manifests, presets, and runtime fallback candidates.

```toml
main = "dist/server/entry.mjs"
assets = ["dist/client", "public"]
```

Asset roots are preset assets plus top-level `assets`, deduplicated, and merged into app `public/` after build. Later copies overwrite earlier files.

## Builds

Use `[build]` for a single stage:

```toml
[build]
install = "bun install"
run = "bun run build"
cwd = "."
include = ["dist/**", "package.json"]
exclude = ["**/*.map"]
```

Use `[[build_stages]]` for multi-stage builds:

```toml
[[build_stages]]
name = "web"
cwd = "apps/web"
install = "bun install"
run = "bun run build"
exclude = ["**/*.map"]

[[build_stages]]
name = "worker"
cwd = "apps/worker"
run = "bun run build"
```

Build stage precedence is:

1. `[[build_stages]]`
2. `[build]`
3. runtime default build
4. no-op

`[build].run` and `[[build_stages]]` are mutually exclusive. `[build].include` and `[build].exclude` cannot be used with `[[build_stages]]`; use per-stage `exclude`.

Builds run in `.tako/build` after copying the source bundle root. Tako respects `.gitignore`, force-excludes `.git/`, `.tako/`, `.env*`, and `node_modules/`, and preserves symlinks as links.

## Variables

Use `[vars]` and `[vars.<env>]` for non-secret strings:

```toml
[vars]
PUBLIC_API_ORIGIN = "https://api.example.com"

[vars.production]
LOG_LEVEL = "warn"
```

Non-string TOML scalar values are stringified. Environment-specific vars override top-level vars. Secrets belong in `tako secrets`, not in `tako.toml`.

## Environments

Each deployable environment lives under `[envs.<name>]`.

```toml
[envs.production]
route = "app.example.com"
servers = ["prod-a"]
source_ip = "auto"
ssl = "letsencrypt"
idle_timeout = 300
storages = { uploads = "prod_uploads" }
backup = { storage = "prod_backups" }
release = ""
```

| Key            | Type         | Meaning                                                                               |
| -------------- | ------------ | ------------------------------------------------------------------------------------- |
| `route`        | string       | Single route pattern. Mutually exclusive with `routes`.                               |
| `routes`       | array        | Multiple route patterns. Mutually exclusive with `route`.                             |
| `servers`      | array        | Server names from global server inventory.                                            |
| `storages`     | inline table | App storage bindings: app-facing name -> top-level resource name or built-in `local`. |
| `backup`       | inline table | App data backup target, currently `{ storage = "resource" }`.                         |
| `source_ip`    | string       | `auto`, `direct`, `cloudflare-proxy`, or `trusted-proxy`.                             |
| `ssl`          | string       | `letsencrypt` or `cloudflare`. Defaults to `letsencrypt`.                             |
| `idle_timeout` | integer      | Seconds before idle scale-to-zero instances stop. Defaults to `300`.                  |
| `release`      | string       | Per-environment release command override. Empty string clears top-level `release`.    |

`development` is reserved for `tako dev` and cannot be deployed.

## Routes

Routes are hostnames with optional path patterns:

```toml
route = "app.example.com"
routes = ["app.example.com", "*.example.com/api/*"]
```

Private/local route hostnames such as `localhost`, `*.localhost`, single-label hosts, and reserved suffixes like `.local`, `.test`, `.invalid`, `.example`, and `.home.arpa` skip ACME and use self-signed certificates.

## Source IP Modes

Generated configs omit `source_ip`, which behaves like `auto`.

| Mode               | Behavior                                                                                                                          |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------- |
| omitted or `auto`  | Use `CF-Connecting-IP` for Cloudflare peers, then configured trusted proxy headers for trusted peers, then the direct peer IP.    |
| `direct`           | Always use the direct TCP peer IP.                                                                                                |
| `cloudflare-proxy` | Require a Cloudflare peer and valid `CF-Connecting-IP`; reject other requests with `403 Forbidden`.                               |
| `trusted-proxy`    | Require loopback or a configured trusted proxy CIDR plus a valid forwarded client IP; reject other requests with `403 Forbidden`. |

For `trusted-proxy`, server-level `trusted_proxy.trusted_cidrs` and optional `trusted_proxy.client_ip_headers` live in `/opt/tako/config.json`, not `tako.toml`.

Forwarded HTTPS metadata uses the same trusted-peer boundary. Direct clients cannot spoof `X-Forwarded-Proto` or `Forwarded: proto=https` to bypass HTTP-to-HTTPS redirects.

## SSL Provider

Public route certificates use Let's Encrypt by default:

```toml
[envs.production]
ssl = "letsencrypt"
```

Wildcard Let's Encrypt routes use Cloudflare DNS-01 and require encrypted provider credentials:

```bash
tako credentials set ssl.cloudflare --env production
```

Cloudflare Origin CA is selected per environment:

```toml
[envs.production]
ssl = "cloudflare"
```

Cloudflare SSL also requires `ssl.cloudflare` credentials. Provider credentials are encrypted in `.tako/secrets.json` under the selected environment and are not exposed to app code.

## Storage Resources

Top-level storage resources contain non-secret S3-compatible metadata:

```toml
[storages.uploads]
provider = "s3"
bucket = "app-uploads"
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"
force_path_style = false
public_base_url = "https://cdn.example.com/uploads"
```

| Key                | Default  | Meaning                                                                                |
| ------------------ | -------- | -------------------------------------------------------------------------------------- |
| `provider`         | `s3`     | Only `s3` is valid in top-level config. The built-in `local` resource is not declared. |
| `bucket`           | required | S3-compatible bucket.                                                                  |
| `endpoint`         | required | HTTPS S3-compatible endpoint.                                                          |
| `region`           | required | Region. Use `auto` for R2.                                                             |
| `force_path_style` | `false`  | Sign path-style URLs instead of virtual-hosted URLs.                                   |
| `public_base_url`  | unset    | HTTPS public base URL for public object helpers.                                       |

App bindings connect environment names to resources:

```toml
[envs.production]
storages = { uploads = "prod_uploads" }
```

The resource name `local` is built in:

```toml
[envs.development]
storages = { uploads = "local" }
```

Do not declare `[storages.local]`. Local storage has no configurable path and no credentials. In development, undeclared storage resources also default to local storage. In deployed environments, every non-`local` binding must reference a declared S3 resource, and `local` can deploy only to single-server environments.

S3 credentials are stored with `tako storages add` or `tako storages credentials`, encrypted in `.tako/secrets.json`, and checked before deploy.

## Backups

Enable app data backups per environment:

```toml
[envs.production]
backup = { storage = "prod_backups" }
```

The backup resource must be a declared private S3-compatible storage resource. `local` and `public_base_url` are rejected for backup storage.

Backup storage is not exposed to app code unless it is also listed under `[envs.<env>].storages`. Tako creates encrypted backup keys when deploy or `tako backups now` first needs them. Archives include app data and durable workflow state, exclude transient channel replay storage, and are stored under `_tako/backups/{app}/{env}/{server}/`.

## Release Commands

A top-level `release` runs once on the leader server before rolling update:

```toml
release = "bun run db:migrate"
```

Per-environment `release` overrides it. An empty string disables the inherited command:

```toml
[envs.staging]
release = ""
```

Release commands run as `sh -c "<command>"` inside the new release directory with non-secret vars and decrypted secrets. If the command fails, deploy aborts on every server, removes partial release directories, leaves `current` untouched, and old instances keep serving.

## Images

`[images]` configures the public optimizer at `/_tako/image`.

```toml
[images]
local_patterns = ["/images/**"]
remote_patterns = ["https://cdn.example.com/uploads/**", "assets.example.com/**"]
sizes = [320, 640, 960, 1200, 1920]
qualities = [75]
formats = ["webp", "avif"]
```

| Key               | Default                       | Meaning                                                                 |
| ----------------- | ----------------------------- | ----------------------------------------------------------------------- |
| `local_patterns`  | `["/**"]`                     | Local public path allowlist. Setting it replaces the default.           |
| `remote_patterns` | `[]`                          | Remote URL allowlist. Protocol-less patterns allow both HTTP and HTTPS. |
| `sizes`           | `[320, 640, 960, 1200, 1920]` | Allowed public output widths.                                           |
| `qualities`       | `[75]`                        | Allowed public quality values, `1..100`.                                |
| `formats`         | `["webp"]`                    | Allowed output formats: `webp`, `avif`.                                 |

Remote sources reject unsupported schemes, userinfo, fragments, recursive optimizer URLs, private/local hosts and IPs, private/local DNS results, and redirects.

## Workflows

Workflow worker config can be global, named, and server-specific:

```toml
[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1

[servers.prod-a.workflows]
workers = 2

[servers.prod-a.workflows.email]
workers = 4
```

`workers` is the number of always-on worker processes. `0` means scale-to-zero. `concurrency` is the maximum parallel runs per worker and defaults to `10`.

Precedence for unnamed workflows is built-in defaults, `[workflows]`, then `[servers.<name>.workflows]`.

Precedence for named workers is built-in defaults, `[workflows]`, `[workflows.<group>]`, `[servers.<name>.workflows]`, then `[servers.<name>.workflows.<group>]`.

## Per-Server App Overrides

`[servers.<name>]` in `tako.toml` is not the global server inventory. It is per-app per-server config. The supported key today is `workflows`.

Global server inventory is managed with `tako servers add/list/remove` and stored outside the project.
