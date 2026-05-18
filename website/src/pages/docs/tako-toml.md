---
layout: ../../layouts/DocsLayout.astro
title: "tako.toml reference for routes, builds, secrets, and scaling - Tako Docs"
heading: "tako.toml Reference"
current: tako-toml
description: "Complete tako.toml reference covering routes, runtime settings, builds, secrets, scaling, environments, and deployment configuration."
---

# `tako.toml` Reference

`tako.toml` is the app config for identity, runtime selection, presets, builds, routes, environment variables, workflow workers, release commands, images, storage bindings, and deployment targets.

App-scoped commands read `./tako.toml` by default. Use `-c` or `--config <CONFIG>` to choose another file. If the value has no `.toml` suffix, Tako appends it. The selected file's parent directory becomes the app directory.

## Minimal Config

```toml
name = "my-app"
runtime = "bun"
preset = "tanstack-start"

[envs.production]
route = "app.example.com"
servers = ["prod-a"]
```

Generated configs stay minimal. Defaults such as `source_ip = "auto"` are not written unless you choose to set them.

## Complete Example

```toml
name = "my-app"
runtime = "bun@1.2.3"
package_manager = "pnpm"
preset = "tanstack-start"
app_root = "src"
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
release = "bun run db:migrate"

dev = ["vite", "dev"]

[build]
install = "pnpm install --frozen-lockfile"
run = "pnpm build"
cwd = "."
include = ["config/*.json"]
exclude = ["tmp/**"]

[vars]
APP_NAME = "my-app"

[vars.production]
LOG_LEVEL = "info"

[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
# local_patterns defaults to ["/**"] when omitted.
sizes = [320, 640, 960, 1200, 1920]
qualities = [75]
formats = ["avif", "webp"]

[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1
concurrency = 5

[envs.development]
routes = ["my-app.test", "*.my-app.test"]
storages = { uploads = "dev_uploads" }

[envs.production]
routes = ["app.example.com", "*.app.example.com"]
servers = ["prod-a", "prod-b"]
storages = { uploads = "prod_uploads" }
source_ip = "cloudflare-proxy"
idle_timeout = 300

[storages.prod_uploads]
provider = "s3"
bucket = "my-app-prod"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
public_base_url = "https://cdn.example.com"

[servers.prod-a.workflows.email]
workers = 2
```

## Top-Level Fields

| Field             | Type            | Meaning                                                                                                      |
| ----------------- | --------------- | ------------------------------------------------------------------------------------------------------------ |
| `name`            | string          | Optional but recommended stable app identity. Defaults to the config file's parent directory name.           |
| `runtime`         | string          | Optional runtime override: `bun`, `node`, or `go`. Add `@<version>` to pin deploys, for example `bun@1.2.3`. |
| `package_manager` | string          | Package manager override for JavaScript runtimes, optionally with a version such as `pnpm@9.1.0`.            |
| `preset`          | string          | Runtime-local preset alias such as `tanstack-start`, `vite`, or `nextjs`.                                    |
| `dev`             | string array    | Custom `tako dev` command. Overrides preset and runtime dev defaults.                                        |
| `app_root`        | string          | JS-only source root for `channels/` and `workflows/`. Defaults to `src`; use `.` for root-level files.       |
| `main`            | string          | Runtime entrypoint override written to deployed `app.json`.                                                  |
| `assets`          | string array    | Extra asset directories merged into deployed `public/`.                                                      |
| `release`         | string          | One-shot release command run on the leader server before rolling update.                                     |
| `build`           | table           | Single build command configuration.                                                                          |
| `build_stages`    | array of tables | Multi-stage build configuration. Mutually exclusive with `build.run`.                                        |
| `vars`            | tables          | Global and per-environment non-secret variables.                                                             |
| `images`          | table           | Public image optimizer allowlists and output options.                                                        |
| `workflows`       | table           | App-wide workflow worker defaults and named worker groups.                                                   |
| `envs`            | tables          | Environment routes, servers, storage bindings, source-IP mode, and scaling policy.                           |
| `storages`        | tables          | Reusable storage resources.                                                                                  |
| `servers`         | tables          | Per-app per-server overrides, currently for workflow workers.                                                |

Unknown top-level keys are rejected.

## App Identity

`name` must be DNS-label-like: lowercase letters, numbers, and hyphens; it must start with a lowercase letter, must not end with a hyphen, and must be at most 63 characters.

The remote deployment id is `{name}/{env}`. Renaming the app creates a new server-side identity. Delete the old deployment manually if needed.

## Runtime And Preset Resolution

`runtime` can be `bun`, `node`, or `go`. If omitted, Tako detects the runtime from project files.

`preset` must be a runtime-local official alias. Do this:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Do not namespace the preset in `tako.toml`:

```toml
# Invalid in tako.toml
preset = "js/tanstack-start"
```

Deploy qualifies the runtime-local alias internally. Unpinned official presets are refreshed from the `master` branch on deploy and fall back to cache if fetching fails. `tako dev` prefers embedded or cached preset data and only fetches when nothing local is available.

## Entrypoints

Tako resolves `main` in this order:

1. Top-level `main`.
2. Runtime manifest main, such as `package.json` `main`.
3. Preset `main`.

For JS runtimes, if a preset points to `index.<ext>` or `src/index.<ext>`, Tako searches `index.ts`, `index.tsx`, `index.js`, `index.jsx`, then matching `src/` files before using the preset fallback.

`app_root` only controls JS channel and workflow discovery. It does not change `main`, `assets`, build paths, package roots, or generated declaration placement.

## Environment Variables

```toml
[vars]
APP_NAME = "my-app"

[vars.production]
LOG_LEVEL = "info"
```

`[vars]` applies to every environment. `[vars.<env>]` overrides or adds values for one environment.

Tako derives `ENV` automatically and ignores user-provided `ENV` in `[vars]` or `[vars.<env>]`. Runtime plugins add runtime vars such as `NODE_ENV` for JavaScript and `BUN_ENV` for Bun.

Secrets do not go in `tako.toml`; use `tako secrets set`.

## Build

Use `[build]` for one build command:

```toml
[build]
install = "pnpm install --frozen-lockfile"
run = "pnpm build"
cwd = "."
include = ["config/*.json"]
exclude = ["tmp/**"]
```

| Field     | Type         | Meaning                                               |
| --------- | ------------ | ----------------------------------------------------- |
| `install` | string       | Optional pre-build install command.                   |
| `run`     | string       | Build command.                                        |
| `cwd`     | string       | Build working directory relative to the project root. |
| `include` | string array | Extra file globs to include in the deploy artifact.   |
| `exclude` | string array | File globs to exclude from the deploy artifact.       |

Use `[[build_stages]]` for multi-stage builds:

```toml
[[build_stages]]
name = "sdk"
cwd = "../sdk/javascript"
run = "bun run build"
exclude = ["node_modules/**"]

[[build_stages]]
name = "app"
run = "bun run build"
```

| Field     | Type         | Meaning                                                                                                     |
| --------- | ------------ | ----------------------------------------------------------------------------------------------------------- |
| `name`    | string       | Optional display label.                                                                                     |
| `cwd`     | string       | Working directory relative to `tako.toml`. May use `..`; deploy guards against escaping the workspace root. |
| `install` | string       | Optional command before `run`.                                                                              |
| `run`     | string       | Required stage command.                                                                                     |
| `exclude` | string array | Per-stage exclusions from the deploy artifact.                                                              |

`[build].run` and `[[build_stages]]` are mutually exclusive. `[build].include` and `[build].exclude` cannot be combined with `[[build_stages]]`.

## Images

`[images]` configures the public optimizer endpoint at `/_tako/image`.

```toml
[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
local_patterns = ["/images/**"]
sizes = [320, 640, 960, 1200, 1920]
qualities = [75]
formats = ["avif", "webp"]
```

| Field             | Default                       | Meaning                                                              |
| ----------------- | ----------------------------- | -------------------------------------------------------------------- |
| `local_patterns`  | `["/**"]`                     | Local public path allowlist. Setting it replaces the default.        |
| `remote_patterns` | `[]`                          | Remote image URL allowlist. Remote images are denied unless matched. |
| `sizes`           | `[320, 640, 960, 1200, 1920]` | Allowed output widths.                                               |
| `qualities`       | `[75]`                        | Allowed output qualities.                                            |
| `formats`         | `["avif", "webp"]`            | Allowed output formats.                                              |

Patterns are glob-like URL strings, not regular expressions. `*` matches one path segment and `**` matches the rest of a path. Remote hosts may use a leading wildcard, and remote patterns without a protocol match both `http` and `https`.

## Environments

```toml
[envs.production]
route = "app.example.com"
servers = ["prod-a"]
storages = { uploads = "prod_uploads" }
source_ip = "cloudflare-proxy"
idle_timeout = 300
release = "bun run db:migrate"
```

| Field          | Type         | Meaning                                                                                      |
| -------------- | ------------ | -------------------------------------------------------------------------------------------- |
| `route`        | string       | Single route. Mutually exclusive with `routes`.                                              |
| `routes`       | string array | Multiple routes. Mutually exclusive with `route`.                                            |
| `servers`      | string array | Global server names from `config.toml` to deploy this environment to.                        |
| `storages`     | map          | App binding name to storage resource name.                                                   |
| `source_ip`    | string       | Optional source-IP mode: `auto`, `direct`, `cloudflare-proxy`, or `trusted-proxy`.           |
| `idle_timeout` | integer      | Seconds before an idle scale-to-zero app can stop. Default `300`.                            |
| `release`      | string       | Per-environment release command override. Empty string clears the top-level release command. |

Non-development environments must define `route` or `routes`. `development` is reserved for `tako dev`; deploy refuses it and ignores servers declared there.

Routes can be exact hosts, wildcard hosts, path-prefixed hosts, or wildcard paths:

```toml
route = "api.example.com"
routes = ["example.com/api/*", "*.example.com/admin/*"]
```

Wildcard hosts must start with `*.`. Path wildcards must be a complete segment, such as `/api/*`.

## Source IP Modes

Generated configs omit `source_ip`, which behaves like `auto`.

| Mode               | Behavior                                                                                                                          |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------- |
| omitted or `auto`  | Use `CF-Connecting-IP` for Cloudflare peers, then configured trusted proxy headers for trusted peers, then the direct peer IP.    |
| `direct`           | Always use the direct TCP peer IP.                                                                                                |
| `cloudflare-proxy` | Require a Cloudflare peer and valid `CF-Connecting-IP`; reject other requests with `403 Forbidden`.                               |
| `trusted-proxy`    | Require loopback or a configured trusted proxy CIDR plus a valid forwarded client IP; reject other requests with `403 Forbidden`. |

For `trusted-proxy`, server-level `trusted_proxy.trusted_cidrs` and optional `trusted_proxy.client_ip_headers` live in `/opt/tako/config.json`, not `tako.toml`.

## DNS Credentials

There is no DNS provider block in `tako.toml`. Wildcard routes require encrypted app-environment DNS credentials:

```bash
tako dns configure --env production
```

Cloudflare is the only supported DNS-01 provider. The token is encrypted in `.tako/secrets.json` under the environment's `dns` object. Deploy sends DNS credentials to servers only when the selected environment has wildcard routes.

## Storage

Storage bindings are split between `tako.toml` and `.tako/secrets.json`:

```toml
[envs.production]
storages = { uploads = "prod_uploads" }

[storages.prod_uploads]
provider = "s3"
bucket = "my-app-prod"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
force_path_style = false
public_base_url = "https://cdn.example.com"
```

| Field              | Type    | Meaning                                                       |
| ------------------ | ------- | ------------------------------------------------------------- |
| `provider`         | string  | S3-compatible resource provider. Defaults to `s3`.            |
| `bucket`           | string  | Required for S3.                                              |
| `endpoint`         | string  | Required HTTPS endpoint for S3-compatible APIs.               |
| `region`           | string  | Required for S3. Use `auto` for R2.                           |
| `force_path_style` | boolean | Use path-style bucket URLs instead of virtual-hosted URLs.    |
| `public_base_url`  | string  | Optional HTTPS public origin/base URL for public object URLs. |

Top-level `[storages.<resource>]` tables are for S3-compatible resources. `provider = "local"` is invalid in config.

`local` is the built-in local storage resource name:

```toml
[envs.production]
storages = { uploads = "local" }
```

It has no `[storages.local]` table, configurable path, or credentials. In `development`, an undeclared storage resource also defaults to local storage. In deploy environments, every bound resource must be declared unless it is `local`.

S3 credentials are stored with `tako storages add`, encrypted in `.tako/secrets.json`, and checked for expiry before deploy.

## Release Commands

Top-level `release` runs once on the leader server before rolling update:

```toml
release = "bun run db:migrate"
```

Override per environment:

```toml
[envs.staging]
release = ""
```

The command runs as `sh -c "<command>"` in the release directory, after production dependencies are installed and before new HTTP instances start. It receives the same non-secret vars and freshly decrypted app secrets for the target environment.

If the release command fails, deploy aborts on every server, removes the partial release directory, leaves `current` untouched, and old instances keep serving.

## Workflows

```toml
[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1
concurrency = 5

[servers.prod-a.workflows.email]
workers = 2
```

| Field         | Meaning                                                               |
| ------------- | --------------------------------------------------------------------- |
| `workers`     | Always-on worker process count. `0` means scale-to-zero. Default `0`. |
| `concurrency` | Max parallel task slots per worker. Default `10`.                     |

Precedence for named worker groups is:

```text
built-in defaults
< [workflows]
< [workflows.<worker>]
< [servers.<name>.workflows]
< [servers.<name>.workflows.<worker>]
```

Worker group names follow server-name rules: lowercase letters, numbers, and hyphens; start with a lowercase letter; no trailing hyphen.

## Server Overrides

`[servers.<name>]` in `tako.toml` is per-app per-server config. It is not the global server inventory. The only supported key today is `workflows`.

Global server inventory is managed in Tako's user config:

```toml
[[servers]]
name = "prod-a"
host = "prod-a.tailnet.ts.net"
```

Use `tako servers add`, `tako servers remove`, and `tako servers list` to manage that file.
