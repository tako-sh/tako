---
layout: ../../layouts/DocsLayout.astro
title: "tako.toml Reference - Tako Docs"
heading: "tako.toml Reference"
current: tako-toml
description: "Complete tako.toml reference covering routes, runtime settings, builds, secrets, scaling, environments, and deployment configuration."
---

# `tako.toml` Reference

`tako.toml` describes one app: identity, runtime, build, routes, variables, storage bindings, backups, SSL provider, source-IP policy, workflows, and target servers. App secrets, storage credentials, and provider credentials are encrypted in `.tako/secrets.json`, not stored in this file.

App-scoped commands use `./tako.toml` by default. `-c path/to/config` selects another config file and treats that file's parent directory as the app directory.

## Minimal Config

```toml
name = "my-app"
runtime = "bun"
preset = "tanstack-start"

[envs.production]
route = "my-app.example.com"
servers = ["prod-a"]
```

`name` is optional but recommended. If omitted, Tako derives the app name from the selected config file's parent directory. Remote identity is `{name}/{env}`, so renaming the app or directory fallback creates a separate deployed app.

## Top-Level Fields

| Field             | Type   | Purpose                                                                                               |
| ----------------- | ------ | ----------------------------------------------------------------------------------------------------- |
| `name`            | string | Stable app identity. Must be DNS-friendly lowercase letters, numbers, and hyphens.                    |
| `runtime`         | string | Runtime adapter, optionally pinned with `@version`, such as `bun@1.2.3`, `node`, or `go`.             |
| `package_manager` | string | JS package manager override: `bun`, `npm`, `pnpm`, or `yarn`.                                         |
| `preset`          | string | Runtime-local preset alias such as `vite`, `tanstack-start`, or `nextjs`.                             |
| `main`            | string | Runtime entrypoint override. May be a file path or module specifier.                                  |
| `app_root`        | string | JS channels/workflows root, relative to `tako.toml`. Defaults to `src`; use `.` for root-level files. |
| `dev`             | array  | Custom `tako dev` command. Overrides preset and runtime defaults.                                     |
| `start`           | array  | Native deploy start command for prebuilt artifacts.                                                   |
| `assets`          | array  | Additional asset directories merged into deployed `public/`.                                          |
| `container`       | string | Container file path for container releases.                                                           |
| `release`         | string | One-shot command run on the leader server before rolling update.                                      |

When `container` is set, native release fields are invalid: `main`, `start`, `assets`, `[build]`, and `[[build_stages]]`. `tako dev` still uses the dev command or runtime/preset dev defaults and does not build the container file locally.

`start` is for native artifacts such as compiled binaries. An exact `{main}` argument is replaced with the resolved entrypoint. The process must still use a Tako SDK listener so fd 3 bootstrap, fd 4 readiness, secrets, storage bindings, and health checks work.

## Routes And Environments

```toml
[envs.production]
route = "app.example.com"
servers = ["la", "nyc"]
idle_timeout = 300
ssl = "letsencrypt"
source_ip = "auto"
storages = { uploads = "prod_uploads" }
backup = { storage = "private_backups" }

[envs.staging]
routes = ["staging.example.com", "example.com/api/*"]
servers = ["staging"]
release = ""
```

Each environment can use either `route` or `routes`, not both. Non-development deploy environments must define at least one route. `development` is reserved for `tako dev`; deploy validation ignores `servers` there.

Routes support exact hosts, wildcard hosts, host-plus-path routes, and wildcard-plus-path routes. Path-only routes are invalid.

Environment tables accept route declarations, `servers`, `storages`, `backup`, `source_ip`, `ssl`, `idle_timeout`, and `release`. Put environment variables in `[vars]` or `[vars.<env>]`, not in `[envs.<env>]`.

`idle_timeout` defaults to 300 seconds. `ssl` defaults to `letsencrypt` and can be `cloudflare`. `source_ip` can be omitted or set to `auto`, `direct`, `cloudflare-proxy`, or `trusted-proxy`.

Environment-level `release` overrides the top-level release command. An empty string clears an inherited top-level command for that environment.

## Variables

```toml
[vars]
API_URL = "https://api.example.com"
FEATURE_FLAG = true

[vars.staging]
API_URL = "https://staging-api.example.com"
```

Variables merge in this order: `[vars]`, then `[vars.<env>]`, then Tako runtime values. TOML strings, numbers, booleans, and datetimes are accepted and converted to process-env strings. Arrays and tables are invalid variable values.

`ENV` is reserved and ignored with a warning if set by the user. Set framework log variables such as `LOG_LEVEL` yourself when needed.

## Build

Simple build:

```toml
[build]
install = "bun install"
run = "bun run build"
cwd = "packages/web"
include = ["dist/**", "package.json"]
exclude = ["**/*.map"]
```

Multi-stage build:

```toml
[[build_stages]]
name = "shared-ui"
cwd = "packages/ui"
install = "bun install"
run = "bun run build"
exclude = ["**/*.map"]

[[build_stages]]
name = "web"
cwd = "apps/web"
run = "bun run build"
```

`[build]` and `[[build_stages]]` are mutually exclusive. Build stage precedence is `[[build_stages]]`, then `[build]`, then the runtime default. JS defaults run the package manager's build script when present. Go defaults build `app`, and also `worker` when `cmd/worker/main.go` exists.

Simple `[build].cwd` must stay inside the project. Multi-stage `cwd` can use `..` for monorepos but cannot escape the source workspace.

## Images

```toml
[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
local_patterns = ["/images/**"]
sizes = [320, 640, 960, 1200, 1920]
qualities = [75]
formats = ["webp", "avif"]
```

Local image paths default to `["/**"]`; setting `local_patterns` replaces that default. Remote images are denied unless they match `remote_patterns`. Patterns are glob-like URL strings: `*` matches one segment and `**` matches the rest of a path. Remote patterns without a protocol match both `http` and `https`.

## Storage

```toml
[envs.production]
storages = { uploads = "prod_uploads" }

[storages.prod_uploads]
provider = "s3"
bucket = "app-uploads"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
force_path_style = false
public_base_url = "https://cdn.example.com/uploads"
```

Top-level storage resources store non-secret metadata. S3 resources require `bucket`, `endpoint`, and `region`; endpoints and public base URLs must use HTTPS. Set `force_path_style = true` when your S3-compatible provider needs path-style bucket URLs instead of virtual-hosted bucket URLs. Credentials are set by `tako storages add` or `tako storages credentials` and stored encrypted in `.tako/secrets.json`.

`local` is a built-in resource name. Bind to it with `storages = { uploads = "local" }`; do not declare `[storages.local]`. Local storage deploys only to single-server environments. In `development`, undeclared storage resources default to local storage.

## Backups

```toml
[envs.production]
backup = { storage = "private_backups" }

[storages.private_backups]
provider = "s3"
bucket = "app-data"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
```

Backup storage must be private S3-compatible storage. `public_base_url` and local storage are rejected for backups. Backup resources are not exposed to `tako.storages` unless they are also listed in `[envs.<env>].storages`. Backup archives preserve symlinks as symlinks.

## SSL Credentials

Use `ssl = "letsencrypt"` for the default. Exact routes can use HTTP-01 without provider credentials. Wildcard Let's Encrypt routes require `ssl.cloudflare` because they use Cloudflare DNS-01. If `ssl.cloudflare` is present, exact Let's Encrypt routes also use DNS-01.

Use `ssl = "cloudflare"` for Cloudflare Origin CA certificates. This also requires `ssl.cloudflare`.

Set provider credentials with:

```bash
tako credentials set ssl.cloudflare --env production
```

Provider credentials are encrypted in `.tako/secrets.json`, not stored in `tako.toml`.

## Workflows

```toml
[workflows]
workers = 0
concurrency = 10

[workflows.email]
run = ["./worker", "email"]
workers = 1

[servers.la.workflows]
workers = 2

[servers.la.workflows.email]
workers = 4
```

`workers = 0` means scale-to-zero workers. `concurrency` defaults to 10. `run` provides an explicit worker command for runtimes that need one; in v0, container releases support one configured workflow `run` command across the base workflow config and named groups. Named workflow groups inherit from `[workflows]`, then can be overridden per group and per server.

In multi-server environments, JS workflows require `postgres_url` unless every workflow opts into local per-server execution. Go workflow deployments require `postgres_url` for multi-server environments. Channels also require `postgres_url` for multi-server deploys.

## Complete Example

```toml
name = "my-app"
runtime = "bun@1.2.3"
package_manager = "bun"
preset = "tanstack-start"
app_root = "src"
release = "bun run db:migrate"

[build]
run = "bun run build"

[vars]
API_URL = "https://api.example.com"

[vars.staging]
API_URL = "https://staging-api.example.com"

[envs.production]
route = "app.example.com"
servers = ["la", "nyc"]
storages = { uploads = "prod_uploads" }
backup = { storage = "private_backups" }
ssl = "letsencrypt"
source_ip = "auto"

[storages.prod_uploads]
provider = "s3"
bucket = "app-uploads"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
public_base_url = "https://cdn.example.com/uploads"

[storages.private_backups]
provider = "s3"
bucket = "app-backups"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
```
