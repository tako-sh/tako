---
layout: ../../layouts/DocsLayout.astro
title: "Framework presets for Next.js, TanStack Start, and more - Tako Docs"
heading: Presets
current: presets
description: "Learn how Tako presets provide framework-specific defaults for entrypoints, static assets, and dev commands across supported frameworks."
---

# Presets

Presets are framework manifests. They give Tako framework defaults for the deployed entrypoint, static asset roots, and local dev command.

Presets are intentionally small. Runtime plugins own production install commands, runtime downloads, package-manager behavior, launch arguments, runtime environment variables, and default build commands.

## What Each Layer Owns

| Layer          | Owns                                                                                                                                                                             |
| -------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Runtime plugin | Runtime id, language, entrypoint candidates, runtime download, package manager, production install, start command, default build command, default dev command, runtime env vars. |
| Preset         | Framework alias, default deployed `main`, default `assets`, optional dev command, and optional runtime-local dev override.                                                       |
| `tako.toml`    | App choice of runtime and preset, explicit overrides, routes, vars, build stages, storage, source-IP mode, and deploy targets.                                                   |

This split keeps presets predictable and keeps runtime behavior consistent across frameworks.

## Selecting A Preset

Use a runtime-local alias in `tako.toml`:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Do not include the runtime namespace:

```toml
# Invalid in tako.toml
preset = "js/tanstack-start"
```

Tako qualifies the alias internally from `runtime`.

Official aliases can be pinned to a commit:

```toml
runtime = "bun"
preset = "tanstack-start@abc1234"
```

Commit pins must be 7 to 64 hexadecimal characters.

## Built-In Manifests

Preset family manifests live under `presets/`:

- `presets/javascript.toml`
- `presets/go.toml`

The current JavaScript manifest contains:

```toml
[vite]
dev = ["vite", "dev"]

[vite.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]

[tanstack-start]
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[tanstack-start.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]

[nextjs]
main = ".next/tako-entry.mjs"
dev = ["next", "dev"]
```

The Bun overrides run Vite directly through Bun's ESM loader so fd-4 readiness keeps working.

The Go family manifest is intentionally empty today. Go's defaults come from the Go runtime plugin:

- deployed main: `app`
- dev command: `go run .`
- build command: `CGO_ENABLED=0 go build -o app .`
- production launch args: `{main}`
- no server-side runtime download

## Resolution Rules

When deploying with an unpinned official preset, Tako fetches the official manifest from the `master` branch and caches it locally. If fetching fails, deploy falls back to cached content.

When running `tako dev`, Tako prefers embedded or cached manifest data and only fetches when nothing local is available.

Base runtime presets such as `bun`, `node`, and `go` may be absent from a family manifest. In that case, Tako uses runtime plugin defaults.

## Preset Schema

Each top-level section defines a preset alias:

```toml
[my-framework]
name = "my-framework"
main = "dist/server/entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[my-framework.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

| Field                    | Type         | Meaning                                                       |
| ------------------------ | ------------ | ------------------------------------------------------------- |
| `name`                   | string       | Optional display/name override. Defaults to the section name. |
| `main`                   | string       | Default deployed entrypoint.                                  |
| `assets`                 | string array | Static asset directories merged into deployed `public/`.      |
| `dev`                    | string array | Custom `tako dev` command for the framework.                  |
| `[preset.<runtime>].dev` | string array | Runtime-specific dev command override.                        |

Runtime override sections only support `dev`. `name`, `main`, and `assets` still come from the base preset section.

## Entrypoints

Tako resolves deployed `main` in this order:

1. Top-level `main` in `tako.toml`.
2. Runtime manifest main, such as `package.json` `main`.
3. Preset `main`.

For JavaScript runtimes, when a preset points to `index.<ext>` or `src/index.<ext>`, Tako searches common root and `src/` entrypoint files before using the preset fallback.

If no entrypoint can be resolved, deploy and dev fail with guidance.

## Assets

Preset `assets` and top-level `assets` in `tako.toml` are combined and deduplicated. Asset directories are merged into deployed `public/`.

```toml
preset = "tanstack-start"
assets = ["public-extra"]
```

With the TanStack Start preset, deploy includes both `dist/client` and `public-extra`.

## Dev Commands

Tako chooses the dev command in this order:

1. Top-level `dev` in `tako.toml`.
2. Runtime-specific preset override, such as `[vite.bun].dev`.
3. Base preset `dev`.
4. Runtime plugin default.

Runtime defaults are useful for simple apps:

- Bun runs through the `tako.sh` Bun server entrypoint.
- Node runs through the `tako.sh` Node server entrypoint with `--experimental-strip-types`.
- Go runs `go run .`.

Framework presets override those defaults when the framework already has its own dev server.

## Customization

Custom preset support is intentionally limited while the protocol is v0. GitHub preset references and `github:` references in `tako.toml` are rejected. Use official aliases and app-level overrides when you need to customize behavior.

Common app-level overrides:

```toml
main = "dist/server/custom-entry.mjs"
assets = ["dist/client", "public"]
dev = ["pnpm", "dev", "--host", "127.0.0.1"]

[build]
run = "pnpm build"
```

These overrides belong in `tako.toml`, not server config.
