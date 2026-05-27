---
layout: ../../layouts/DocsLayout.astro
title: "Framework Presets - Tako Docs"
heading: Presets
current: presets
description: "Learn how Tako presets provide framework-specific defaults for entrypoints, static assets, and dev commands across supported frameworks."
---

# Presets

Presets are framework manifests. They give Tako default entrypoints, static asset roots, and development commands so `tako.toml` can stay small.

Presets do not store secrets, routes, servers, storage credentials, or deployment policy. Those stay in `tako.toml` and `.tako/secrets.json`.

## How Presets Fit

| Layer          | What it controls                                                                                                                     |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| Runtime plugin | Base runtime behavior: entrypoint candidates, default build/install/start/dev commands, package-manager behavior, runtime downloads. |
| Preset         | Framework defaults: `main`, `assets`, and `dev`.                                                                                     |
| `tako.toml`    | App choices and overrides: runtime, preset, build, routes, vars, storage, backups, SSL, source-IP, workflows, and target servers.    |

The app config always wins over preset defaults. Set `main`, `assets`, `dev`, or `[build]` in `tako.toml` when a project needs a different shape.

## Built-In Presets

Built-in presets are grouped by runtime family.

| Family     | Preset           | Defaults                                                                                        |
| ---------- | ---------------- | ----------------------------------------------------------------------------------------------- |
| JavaScript | `vite`           | Dev command `vite dev`.                                                                         |
| JavaScript | `tanstack-start` | Main `dist/server/tako-entry.mjs`, assets `dist/client`, dev command `vite dev`.                |
| JavaScript | `nextjs`         | Main `.next/tako-entry.mjs`, dev command `next dev`.                                            |
| Go         | none today       | The Go runtime base preset builds `CGO_ENABLED=0 go build -o app .` and runs `go run .` in dev. |

Example:

```toml
runtime = "bun"
preset = "tanstack-start"
```

## Runtime-Local Aliases

Preset names are selected within the runtime family. Do not namespace them:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Not:

```toml
preset = "js/tanstack-start"
```

The runtime chooses the family. A Bun app and a Node app can use the same JavaScript preset name while still getting runtime-specific command behavior from the selected runtime.

## Runtime-Specific Dev Overrides

Preset manifests can define runtime-local override sections. The current JavaScript presets use Bun-specific Vite commands so Vite runs under Bun's ESM loader and keeps Tako's fd-4 readiness handshake intact.

```toml
[vite]
dev = ["vite", "dev"]

[vite.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Only the nested override's `dev` field replaces the base preset dev command. Other base preset fields, such as `main` and `assets`, still come from the main preset section.

## Resolution Order

During deploy, Tako resolves framework behavior in this order:

1. Runtime plugin defaults.
2. Selected preset defaults.
3. Explicit `tako.toml` overrides.

Entrypoint resolution follows the same spirit:

1. `main` in `tako.toml`.
2. Manifest main such as `package.json` `main`.
3. Preset `main`.
4. Runtime entrypoint candidates such as `index.ts`, `index.js`, `src/index.ts`, or `main.go`.

Deploy verifies the resolved `main` exists in the built app directory before packaging the release.

## Build Interaction

Presets may provide defaults, but build stages are controlled by app config:

```toml
[build]
run = "bun run build"
```

or:

```toml
[[build_stages]]
name = "client"
run = "bun run build:client"

[[build_stages]]
name = "server"
run = "bun run build:server"
```

Build precedence is `[[build_stages]]`, then `[build]`, then the runtime default build, then no-op. Preset assets are merged with top-level `assets` and copied into `public/` after build.

## Init Behavior

`tako init` detects a runtime, fetches official preset family manifests in interactive mode, and writes a compact config:

- Base runtime adapters leave `preset` commented or unset.
- Framework presets write `preset = "<name>"`.
- `main` is written only when inference finds a project entrypoint that differs from the preset default, or when no preset/runtime default can supply one.
- JavaScript projects install `tako.sh` with the selected package manager.
- Go projects run `go get tako.sh`.

If no family presets are available after fetch, init skips preset selection and uses the runtime base behavior.

## Custom Preset Manifests

Preset manifest files live at `presets/<language>.toml`. Each top-level table is one preset alias.

```toml
[my-framework]
main = "dist/server/entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[my-framework.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Supported preset fields:

| Field    | Meaning                                                  |
| -------- | -------------------------------------------------------- |
| `name`   | Optional display name. Defaults to the table name.       |
| `main`   | Default runtime entrypoint.                              |
| `assets` | Static asset directories merged into deployed `public/`. |
| `dev`    | Dev command for `tako dev`.                              |

The example manifest in `presets/_example.toml` is the schema reference for current preset files.

## When To Override Instead

Use `tako.toml` overrides when the behavior is project-specific:

```toml
runtime = "bun"
preset = "vite"
main = "server/entry.ts"
assets = ["dist/client", "public"]
dev = ["bun", "run", "dev"]
```

Create or edit a preset only when several projects share the same framework shape.
