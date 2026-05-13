---
layout: ../../layouts/DocsLayout.astro
title: "tako.toml reference for routes, builds, secrets, and scaling - Tako Docs"
heading: "tako.toml Reference"
current: tako-toml
description: "Complete tako.toml reference covering routes, runtime settings, builds, secrets, scaling, environments, and deployment configuration."
---

# `tako.toml` Reference

`tako.toml` is the project config for app identity, runtime selection, presets, builds, routes, environment variables, workflow workers, release commands, and deployment targets.

App-scoped commands read `./tako.toml` by default. Use `-c` or `--config <CONFIG>` to choose another file. If the value has no `.toml` suffix, Tako appends it. The selected config file's parent directory is the app directory.

## Complete Example

```toml
name = "dashboard"
runtime = "bun"
runtime_version = "1.2.3"
package_manager = "bun"
preset = "tanstack-start"
main = "dist/server/tako-entry.mjs"
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
release = ""

[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1

[servers.la.workflows]
workers = 2

[servers.la.workflows.email]
workers = 4
```

Use either `[build]` or `[[build_stages]]`, not both:

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
exclude = ["dist/**/*.map"]
```

## App Identity

```toml
name = "dashboard"
```

`name` is optional but recommended. If omitted, Tako derives the app name from the selected config file's parent directory.

Names must:

- start with a lowercase letter
- contain only lowercase letters, numbers, and hyphens
- be DNS-hostname friendly

Remote deployments are stored as `{app}/{env}`. Renaming `name` creates a new deployment identity; remove the old deployment manually when needed.

## Runtime Fields

```toml
runtime = "bun"
runtime_version = "1.2.3"
package_manager = "pnpm"
preset = "tanstack-start"
main = "dist/server/tako-entry.mjs"
dev = ["vite", "dev"]
assets = ["dist/client"]
```

| Field             | Meaning                                                                   |
| ----------------- | ------------------------------------------------------------------------- |
| `runtime`         | Optional runtime override: `bun`, `node`, or `go`.                        |
| `runtime_version` | Optional pinned runtime version used by deploy.                           |
| `package_manager` | Optional JS package manager override: `bun`, `npm`, `pnpm`, or `yarn`.    |
| `preset`          | Optional runtime-local preset alias such as `tanstack-start` or `nextjs`. |
| `app_root`        | JS-only app source root for `channels/` and `workflows/`.                 |
| `main`            | Optional runtime entrypoint override.                                     |
| `dev`             | Optional command override for `tako dev`.                                 |
| `assets`          | Extra asset directories copied into deployed `public/`.                   |

`app_root` is relative to `tako.toml` and defaults to `src`. Use `app_root = "."` when JavaScript app files live next to `tako.toml`. It controls where Tako discovers `channels/` and `workflows/`; it does not change `main`, `assets`, generated declaration placement, build paths, or deploy packaging roots.

When `main` is omitted, Tako checks manifest metadata such as `package.json` `main`, then preset defaults, then runtime defaults. JS runtimes also look for common index files such as `index.ts`, `index.js`, and `src/index.ts`.

`runtime_version` is used directly when set. Otherwise deploy runs `<runtime> --version` locally and falls back to `latest`.

## Presets

Presets provide framework defaults for `main`, `assets`, and `dev`. They do not contain build commands, production start commands, install commands, or runtime download rules.

Supported runtime-local aliases include:

- `vite`
- `tanstack-start`
- `nextjs`

Pinned aliases can include a commit:

```toml
preset = "tanstack-start@abc1234"
```

Do not include runtime namespaces in `tako.toml` presets. Use `runtime = "bun"` plus `preset = "tanstack-start"`, not `preset = "javascript/tanstack-start"`.

## Variables

```toml
[vars]
API_URL = "https://api.example.com"

[vars.production]
API_URL = "https://api.example.com"
```

Variables merge in this order:

1. `[vars]`
2. `[vars.<environment>]`
3. Tako-derived vars and runtime vars

Tako sets `ENV` automatically and ignores user-defined `ENV` values with a warning. Other names, including framework log-level variables, are user-owned.

Common runtime vars:

- `ENV`
- `TAKO_BUILD` on deploy
- `TAKO_DATA_DIR`
- `TAKO_APP_ROOT` for JS apps
- `NODE_ENV` for JS runtimes
- `BUN_ENV` for Bun
- `PORT=0` and `HOST=127.0.0.1` for HTTP processes
- `TAKO_APP_NAME`
- `TAKO_INTERNAL_SOCKET`

In production, app and worker processes do not inherit arbitrary `tako-server` service env vars. Tako preserves only minimal process env (`PATH`, `HOME` when available), then applies app/runtime vars.

Secrets are not configured in `tako.toml`. Use `tako secrets` commands; local encrypted secret metadata lives in `.tako/secrets.json`.

## Environments

```toml
[envs.production]
route = "dashboard.example.com"
servers = ["la", "nyc"]
idle_timeout = 300
release = "bun run db:migrate"
```

| Field          | Meaning                                                   |
| -------------- | --------------------------------------------------------- |
| `route`        | Single route pattern. Mutually exclusive with `routes`.   |
| `routes`       | Multiple route patterns. Mutually exclusive with `route`. |
| `servers`      | Server names from global `config.toml`.                   |
| `idle_timeout` | Per-instance idle timeout in seconds. Default: `300`.     |
| `release`      | Environment-specific release command override.            |

Non-development environments must define `route` or `routes`. `development` is reserved for `tako dev`; deploy ignores `servers` there.

Routes can be exact hosts, wildcard hosts, or host plus path prefix:

```toml
routes = [
  "example.com",
  "www.example.com",
  "*.example.com/admin/*",
  "example.com/api/*",
]
```

## Build

Simple build mode:

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
- `cwd`: working directory relative to the project root; absolute paths and `..` are rejected
- `include`: artifact include globs
- `exclude`: artifact exclude globs

Multi-stage build mode:

```toml
[[build_stages]]
name = "shared-ui"
cwd = "packages/ui"
install = "bun install"
run = "bun run build"
exclude = ["**/*.map"]
```

Stage fields:

- `name`: optional display label
- `cwd`: relative stage working directory; `..` is allowed for monorepos but guarded against escaping the workspace root
- `install`: optional preparatory command
- `run`: required command
- `exclude`: stage-specific artifact excludes

Build stage precedence:

1. `[[build_stages]]`
2. `[build]`
3. runtime default
4. no-op

`[build]` and `[[build_stages]]` are mutually exclusive when `[build].run` is set. `[build].include` and `[build].exclude` cannot be used with `[[build_stages]]`; use per-stage `exclude`.

## Release Command

```toml
release = "bun run db:migrate"

[envs.staging]
release = ""
```

Top-level `release` runs once on the leader server, after artifact extract and production install, before rolling update. The leader is the first server listed in `[envs.<env>].servers`.

`[envs.<env>].release` overrides the top-level value. An empty string clears the inherited command for that environment.

The command runs as `sh -c` in the new release directory. It receives app env, secrets for that deploy, `TAKO_BUILD`, `TAKO_DATA_DIR`, and `PATH` when no app/release env already supplied it. It runs from a cleared service environment and has a 10-minute timeout.

If the release command fails or times out, deploy aborts, cleans up the partial release directory, and leaves old instances serving.

## Workflow Workers

```toml
[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1

[servers.la.workflows]
workers = 2

[servers.la.workflows.email]
workers = 4
```

Fields:

- `workers`: always-on worker processes. `0` means scale-to-zero. Default: `0`.
- `concurrency`: max parallel runs per worker. Default: `10`.

Precedence for unnamed workflows:

1. built-in defaults
2. `[workflows]`
3. `[servers.<name>.workflows]`

Precedence for `worker: "email"`:

1. built-in defaults
2. `[workflows]`
3. `[workflows.email]`
4. `[servers.<name>.workflows]`
5. `[servers.<name>.workflows.email]`

## Server Overrides

The project config can contain per-server workflow overrides under `[servers.<name>]`. Server inventory itself is not stored here; it lives in global `config.toml` managed by `tako servers add`.

```toml
[servers.la.workflows]
workers = 2
```

## Deploy Artifact Rules

Deploy packages from the git root when available, otherwise from the app directory. The selected config file's parent directory becomes the app subdirectory within that source root.

Tako always excludes:

- `.git/`
- `.tako/`
- `.env*`
- `node_modules/`

Additional excludes come from config and `.gitignore`.

Assets from presets and top-level `assets` are merged into app `public/` after build. Later entries overwrite earlier ones.
