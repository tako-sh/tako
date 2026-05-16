---
layout: ../../layouts/DocsLayout.astro
title: "tako.toml reference for routes, builds, secrets, and scaling - Tako Docs"
heading: "tako.toml Reference"
current: tako-toml
description: "Complete tako.toml reference covering routes, runtime settings, builds, secrets, scaling, environments, and deployment configuration."
---

# `tako.toml` Reference

`tako.toml` is the project config for app identity, runtime selection, presets, builds, routes, environment variables, workflow workers, release commands, and deployment targets.

App-scoped commands read `./tako.toml` by default. Use `-c` or `--config <CONFIG>` to choose another file. If the value has no `.toml` suffix, Tako appends it. The selected file's parent directory is the app directory.

## Complete Example

```toml
name = "dashboard"
runtime = "bun"
runtime_version = "1.2.3"
package_manager = "bun"
preset = "tanstack-start"
app_root = "src"
main = "dist/server/tako-entry.mjs"
dev = ["vite", "dev"]
assets = ["dist/client"]
release = "bun run db:migrate"

[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
sizes = [320, 640, 960, 1200, 1920]
qualities = [75]
formats = ["avif", "webp"]

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
storages = { uploads = "prod_uploads" }
idle_timeout = 300

[envs.staging]
routes = ["staging.example.com", "example.com/staging/*"]
servers = ["staging"]
storages = { uploads = "staging_uploads" }
idle_timeout = 120
release = ""

[storages.prod_uploads]
provider = "s3"
bucket = "dashboard-prod-uploads"
endpoint = "https://s3.example.com"
region = "us-east-1"
public_base_url = "https://cdn.example.com/uploads"

[storages.staging_uploads]
provider = "s3"
bucket = "dashboard-staging-uploads"
endpoint = "https://s3.example.com"
region = "us-east-1"

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
exclude = ["**/*.test.ts"]
```

## App Identity

| Field  | Type   | Meaning                                                                |
| ------ | ------ | ---------------------------------------------------------------------- |
| `name` | string | Optional but recommended stable app identity for dev and deploy paths. |

If `name` is omitted, Tako derives the app name from the selected config file's parent directory. The remote deployment id is `{name}/{env}`, so renaming the app creates a new server-side identity.

Names must:

- start with a lowercase letter
- contain only lowercase letters, numbers, and hyphens
- end with a lowercase letter or number
- be 63 characters or fewer

## Runtime And Entrypoint

| Field             | Type     | Meaning                                                                       |
| ----------------- | -------- | ----------------------------------------------------------------------------- |
| `runtime`         | string   | Optional runtime override: `bun`, `node`, or `go`.                            |
| `runtime_version` | string   | Optional pinned runtime version. Deploy otherwise runs `<runtime> --version`. |
| `package_manager` | string   | Optional JS package manager override: `bun`, `npm`, `pnpm`, or `yarn`.        |
| `preset`          | string   | Optional runtime-local preset alias such as `tanstack-start` or `nextjs`.     |
| `main`            | string   | Optional runtime entrypoint override written to deployed `app.json`.          |
| `app_root`        | string   | JS-only root for `channels/` and `workflows/`; defaults to `src`.             |
| `dev`             | string[] | Optional custom command for `tako dev`.                                       |
| `assets`          | string[] | Extra asset directories merged into deployed `public/`.                       |
| `release`         | string   | Optional command run once on the leader server before rolling update.         |

`main` can be a file path or module specifier. If omitted, Tako checks the runtime manifest main field, then preset `main`.

For JS runtimes, when a preset points to `index.<ext>` or `src/index.<ext>`, deploy/dev look for `index.ts`, `index.tsx`, `index.js`, `index.jsx`, then the matching `src/` files before using the preset fallback.

`app_root` only affects JS channel and workflow discovery. It does not change `main`, `assets`, build paths, deploy packaging roots, or generated declaration placement.

## Presets

Presets are metadata-only:

- `main`
- `assets`
- `dev`
- optional `name`

They do not define install commands, production start commands, runtime downloads, or build behavior.

Valid config:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Invalid config:

```toml
preset = "js/tanstack-start"
preset = "github:owner/repo/path"
```

Use top-level `runtime` to choose the runtime family, then use a runtime-local alias for `preset`.

## Build

### Single Stage

```toml
[build]
install = "bun install"
run = "bun run build"
cwd = "packages/web"
include = ["**/*"]
exclude = ["**/*.map"]
```

| Field     | Type     | Meaning                                                      |
| --------- | -------- | ------------------------------------------------------------ |
| `run`     | string   | Build command.                                               |
| `install` | string   | Optional command run before `run`.                           |
| `cwd`     | string   | Optional working directory relative to the project root.     |
| `include` | string[] | Artifact include globs. Defaults to `["**/*"]` when omitted. |
| `exclude` | string[] | Artifact exclude globs.                                      |

`build.cwd`, include globs, exclude globs, and asset paths must be relative and cannot contain `..`.

### Multi Stage

```toml
[[build_stages]]
name = "web"
cwd = "packages/web"
install = "bun install"
run = "bun run build"
exclude = ["**/*.map"]
```

| Field     | Type     | Meaning                                                   |
| --------- | -------- | --------------------------------------------------------- |
| `name`    | string   | Optional display label.                                   |
| `cwd`     | string   | Optional stage working directory relative to `tako.toml`. |
| `install` | string   | Optional command run before `run`.                        |
| `run`     | string   | Required stage command.                                   |
| `exclude` | string[] | Per-stage artifact excludes.                              |

`[[build_stages]].cwd` can use `..` for monorepos, but deploy guards it from escaping the workspace root.

Build stage precedence:

1. `[[build_stages]]`
2. `[build]`
3. runtime default build
4. no-op

JS runtime defaults run the package manager's `run --if-present build`. Go defaults to `CGO_ENABLED=0 go build -o app .`.

## Variables

```toml
[vars]
API_URL = "https://api.example.com"

[vars.production]
API_URL = "https://api.example.com"
```

Merge order:

1. `[vars]`
2. `[vars.<env>]`
3. Tako runtime variables

`ENV` is reserved. If you set it in `[vars]` or `[vars.<env>]`, Tako ignores it and prints a warning.

Common Tako variables include:

| Name            | Meaning                                         |
| --------------- | ----------------------------------------------- |
| `ENV`           | Active environment.                             |
| `TAKO_BUILD`    | Deployed build id.                              |
| `TAKO_DATA_DIR` | Persistent app-owned data directory.            |
| `TAKO_APP_ROOT` | JS app root for channel and workflow discovery. |
| `NODE_ENV`      | Set for JS runtimes.                            |
| `BUN_ENV`       | Set for Bun.                                    |

Secrets do not live in `tako.toml`; use `tako secrets`.

## Storage

```toml
[envs.production]
storages = { uploads = "prod_uploads" }

[storages.prod_uploads]
provider = "s3"
bucket = "app-uploads"
endpoint = "https://s3.example.com"
region = "us-east-1"
public_base_url = "https://cdn.example.com/uploads"
```

`[envs.<env>].storages` maps app-facing binding names to storage resource names. The key is exposed to app code as `tako.storages.<key>`; the value references a top-level `[storages.<resource>]`.

Supported providers are `s3` and `local`. `s3` requires `bucket`, `endpoint`, and `region`; `endpoint` and optional `public_base_url` must use HTTPS. R2 uses `provider = "s3"` with the R2 S3-compatible endpoint. `local` has no configurable path or credentials. In `development`, an undeclared storage resource defaults to local storage under the app data directory. In deploy environments, every bound resource must be declared.

## Images

```toml
[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
# local_patterns = ["/images/**"]
# sizes = [320, 640, 960, 1200, 1920]
# qualities = [75]
# formats = ["avif", "webp"]
```

Public image URLs use `/_tako/image?src=...&w=...`. Local public paths are allowed by default with `local_patterns = ["/**"]`; setting `local_patterns` replaces that default. Remote images are denied unless their URL matches `remote_patterns`; patterns without a protocol allow both `http` and `https`. JavaScript apps can use `imageUrl` for one optimized URL or `imageSrcSet` for plain `<img>` responsive sources.

Patterns are glob-like strings, not regular expressions. `*` matches one path segment, `**` matches the rest of a path, and remote hosts may use a leading wildcard such as `https://*.example.com/uploads/**`. Remote patterns without a protocol allow both `http` and `https`.

| Field             | Type     | Meaning                                           |
| ----------------- | -------- | ------------------------------------------------- |
| `local_patterns`  | string[] | Optional local path allowlist. Defaults to `/**`. |
| `remote_patterns` | string[] | Remote URL allowlist. Defaults to empty.          |
| `sizes`           | number[] | Allowed public optimizer widths.                  |
| `qualities`       | number[] | Allowed public optimizer qualities.               |
| `formats`         | string[] | Allowed output formats: `avif`, `webp`.           |

## Environments

```toml
[envs.production]
route = "dashboard.example.com"
servers = ["la"]
idle_timeout = 300

[envs.staging]
routes = ["staging.example.com", "*.staging.example.com/admin/*"]
servers = ["staging"]
release = ""
```

| Field          | Type     | Meaning                                                              |
| -------------- | -------- | -------------------------------------------------------------------- |
| `route`        | string   | Single route. Mutually exclusive with `routes`.                      |
| `routes`       | string[] | Multiple routes. Mutually exclusive with `route`.                    |
| `servers`      | string[] | Server names from global `config.toml`.                              |
| `idle_timeout` | number   | Seconds before idle instances stop. Default: `300`.                  |
| `release`      | string   | Per-env release command override. Empty string clears top-level one. |

Non-development environments must define at least one route. `development` is reserved for `tako dev`; deploy refuses it and ignores servers declared there.

Routes support exact hosts, wildcard hosts, host plus path, and wildcard host plus path:

```toml
routes = [
  "example.com",
  "*.example.com",
  "example.com/api/*",
  "*.example.com/admin/*",
]
```

Environment variables belong in `[vars]` and `[vars.<env>]`, not under `[envs.<env>]`.

## Release Commands

Top-level `release` runs for every environment unless overridden:

```toml
release = "bun run db:migrate"

[envs.staging]
release = ""
```

Deploy runs the release command once on the leader server after extract and production install, but before rolling update. It runs with the same app environment that new HTTP instances receive, plus decrypted secrets as env vars for that one-shot command. Timeout is 10 minutes.

If the release command fails, deploy aborts on every server, removes the partial release directory, leaves `current` untouched, and old instances keep serving.

## Workflows

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

| Field         | Meaning                                                                |
| ------------- | ---------------------------------------------------------------------- |
| `workers`     | Always-on worker process count. `0` means scale-to-zero. Default: `0`. |
| `concurrency` | Max parallel runs per worker. Default: `10`.                           |

Unnamed workflow precedence:

1. built-in defaults
2. `[workflows]`
3. `[servers.<name>.workflows]`

Named worker precedence:

1. built-in defaults
2. `[workflows]`
3. `[workflows.<worker>]`
4. `[servers.<name>.workflows]`
5. `[servers.<name>.workflows.<worker>]`

Worker group names use the same name rules as apps and servers.

## Per-Server Overrides

Project `tako.toml` can contain per-server workflow overrides under `[servers.<name>]`. The server inventory itself is not stored here. Global `config.toml` is managed by `tako servers add` and stores host, SSH port, public HTTP/HTTPS ports, description, and target metadata.

```toml
[servers.la.workflows]
workers = 2
```

`[servers.workflows]` is invalid. Use top-level `[workflows]` for app-wide workflow settings, or `[servers.<name>.workflows]` for a specific server.
