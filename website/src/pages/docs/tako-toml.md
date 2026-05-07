---
layout: ../../layouts/DocsLayout.astro
title: "tako.toml reference for routes, builds, secrets, and scaling - Tako Docs"
heading: "tako.toml Reference"
current: tako-toml
description: "Complete tako.toml reference covering routes, runtime settings, builds, secrets, scaling, environments, and deployment configuration."
---

# `tako.toml` Reference

`tako.toml` is the project config for app identity, runtime selection, build settings, routes, environment variables, workflow workers, and deployment targets.

App-scoped commands read `./tako.toml` by default. Use `-c` / `--config <CONFIG>` to select another file. If the path does not end in `.toml`, Tako appends it. The selected config file's parent directory is the app directory.

## Complete Example

```toml
name = "dashboard"
runtime = "bun"
runtime_version = "1.2.3"
package_manager = "bun"
preset = "tanstack-start"
dev = ["vite", "dev"]
assets = ["dist/client"]
release = "bun run db:migrate"

[build]
install = "bun install"
run = "bun run build"
cwd = "packages/web"
include = ["**/*"]
exclude = ["**/*.map"]

[vars]
API_URL = "https://api.example.com"

[vars.production]
API_URL = "https://api.example.com"

[vars.staging]
API_URL = "https://staging-api.example.com"

[envs.production]
route = "dashboard.example.com"
servers = ["la", "nyc"]
idle_timeout = 300

[envs.staging]
routes = ["staging.example.com", "example.com/staging/*"]
servers = ["staging"]
idle_timeout = 120

[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1

[servers.la.workflows]
workers = 2
```

## App Identity

```toml
name = "dashboard"
```

`name` is optional but recommended. If omitted, Tako derives the app name from the selected config file's parent directory.

Names must:

- start with a lowercase letter
- use only lowercase letters, numbers, and hyphens
- be DNS-friendly

Remote deployments use `{name}/{env}` as the server-side identity. Renaming an app creates a different deployment identity; delete the old deployment manually if needed.

## Runtime Fields

```toml
runtime = "bun"
runtime_version = "1.2.3"
package_manager = "pnpm"
preset = "tanstack-start"
main = "dist/server/tako-entry.mjs"
```

`runtime` selects the runtime plugin. Supported values are `bun`, `node`, and `go`.

`runtime_version` pins the runtime version used during deploy. If omitted, deploy runs `<runtime> --version` locally and falls back to `latest`.

`package_manager` overrides JavaScript package-manager detection. If omitted, Tako checks `package.json` `packageManager`, then lockfiles.

`preset` provides framework defaults for `main`, `assets`, and `dev`.

`main` overrides the runtime entrypoint. When omitted, Tako checks manifest main, then preset main, then JavaScript index fallbacks where applicable.

## Dev Command

```toml
dev = ["vite", "dev"]
```

`dev` overrides the command used by `tako dev`. Resolution order:

1. top-level `dev`
2. preset `dev`
3. runtime default

Direct Vite dev commands must use the `tako.sh/vite` plugin so the dev server can signal readiness to Tako over fd 4.

## Builds

Use `[build]` for one build stage:

```toml
[build]
install = "bun install"
run = "bun run build"
cwd = "packages/web"
include = ["**/*"]
exclude = ["**/*.map"]
```

Fields:

- `install`: optional command run before `run`
- `run`: build command
- `cwd`: optional working directory relative to the app root
- `include`: artifact include globs
- `exclude`: artifact exclude globs

Use `[[build_stages]]` for multiple stages:

```toml
[[build_stages]]
name = "shared-ui"
cwd = "packages/ui"
install = "bun install"
run = "bun run build"
exclude = ["**/*.map"]

[[build_stages]]
name = "web"
cwd = "packages/web"
run = "bun run build"
```

`[build]` and `[[build_stages]]` are mutually exclusive when `[build].run` is set. `[build].include` and `[build].exclude` cannot be used with `[[build_stages]]`; use per-stage `exclude` instead.

Build stage precedence:

1. `[[build_stages]]`
2. `[build]`
3. runtime default
4. no-op

Deploy copies the source bundle into `.tako/build`, symlinks local `node_modules`, runs build commands there, merges asset roots into `public/`, verifies `main`, and archives the result without `node_modules`.

## Assets

```toml
assets = ["dist/client"]
```

Asset roots are preset `assets` plus top-level `assets`, deduplicated in order. They are merged into the deployed app's `public/` directory after build. Later entries overwrite earlier files.

## Environment Variables

```toml
[vars]
API_URL = "https://api.example.com"

[vars.production]
API_URL = "https://api.example.com"
```

Merge order:

1. `[vars]`
2. `[vars.<environment>]`
3. Tako runtime vars

Tako sets `ENV` in dev and deploy. `ENV` is reserved; if you set it in `[vars]`, Tako ignores it and prints a warning.

Common runtime vars include:

- `ENV`
- `TAKO_BUILD` on deploy
- `TAKO_DATA_DIR`
- `NODE_ENV` for JavaScript runtimes
- `BUN_ENV` for Bun

Secrets do not live in `tako.toml`; use `tako secrets`.

## Environments and Routes

```toml
[envs.production]
route = "dashboard.example.com"
servers = ["la", "nyc"]
idle_timeout = 300

[envs.preview]
routes = ["preview.example.com", "example.com/preview/*"]
```

Each non-development environment must define `route` or `routes`, not both.

Route patterns can be:

- `api.example.com`
- `*.example.com`
- `example.com/api/*`
- `*.example.com/admin/*`

`development` is reserved for `tako dev`. It may define dev routes, but deploy ignores `servers` in that environment. `.test` and `.tako.test` routes are managed by Tako's local DNS. External development routes are accepted as additional host aliases, but you must point those hostnames at the dev proxy yourself. Unknown `.local` LAN hosts and unknown external hosts get a generic `Misdirected Request` 421 response without route details.

`idle_timeout` is per-instance idle timeout in seconds. Default is `300`.

## Server Membership

```toml
[envs.production]
servers = ["la", "nyc"]
```

Environment server names refer to global servers in `config.toml`, managed by `tako servers add`, `tako servers rm`, and `tako servers ls`.

The same server can host multiple environments. Each environment deploys to its own identity and path under `/opt/tako/apps/{app}/{env}`.

## Release Command

```toml
release = "bun run db:migrate"

[envs.staging]
release = ""
```

`release` runs once on the leader server after artifact extraction and production dependency install, before rolling update.

`[envs.<env>].release` overrides the top-level command. An empty string clears the inherited command for that environment.

The command runs as `sh -c` in the new release directory with normal app env, secrets, `TAKO_BUILD`, and `TAKO_DATA_DIR`. It has a 10-minute timeout. If it fails or times out, deploy aborts, the timed-out process is killed, and old instances keep serving.

## Workflow Workers

```toml
[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 2

[servers.la.workflows]
workers = 1

[servers.la.workflows.email]
workers = 4
```

Fields:

- `workers`: always-on worker process count. `0` means scale to zero.
- `concurrency`: max parallel runs per worker. Default is `10`.

Precedence for default workers:

1. built-in defaults
2. `[workflows]`
3. `[servers.<name>.workflows]`

Precedence for a named worker group:

1. built-in defaults
2. `[workflows]`
3. `[workflows.<group>]`
4. `[servers.<name>.workflows]`
5. `[servers.<name>.workflows.<group>]`

## Per-Server Overrides

`[servers.<name>]` currently supports workflow overrides:

```toml
[servers.la.workflows]
workers = 2
```

Other per-server runtime settings are not part of the `tako.toml` schema.

## Validation Notes

Tako rejects:

- unknown top-level keys
- empty `main`, `runtime`, or `preset`
- unsupported `runtime` values
- namespaced or `github:` preset references in `tako.toml`
- absolute asset paths or build globs
- `..` in asset paths, build globs, or `[build].cwd`
- non-development environments without routes
- both `route` and `routes` in one environment
- `idle_timeout = 0`
