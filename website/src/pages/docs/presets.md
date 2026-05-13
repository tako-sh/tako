---
layout: ../../layouts/DocsLayout.astro
title: "Framework presets for Next.js, TanStack Start, and more - Tako Docs"
heading: Presets
current: presets
description: "Learn how Tako presets provide framework-specific defaults for entrypoints, static assets, and dev commands across supported frameworks."
---

# Presets

Presets are small framework manifests. They help Tako choose the deployed entrypoint, static asset roots, and local dev command for common frameworks.

Presets are deliberately limited. Runtime plugins, not presets, define production install commands, start commands, runtime downloads, package-manager behavior, and runtime environment variables.

## What Presets Can Set

| Field    | Meaning                                                   |
| -------- | --------------------------------------------------------- |
| `name`   | Optional display/name override. Defaults to section name. |
| `main`   | Runtime entrypoint after build.                           |
| `assets` | Static asset directories copied into deployed `public/`.  |
| `dev`    | Command used by `tako dev` before runtime defaults.       |

Example family manifest section:

```toml
[my-framework]
main = "dist/server/entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]
```

## Built-In Presets

JavaScript presets live in `presets/javascript.toml`.

### `vite`

```toml
[vite]
dev = ["vite", "dev"]
```

The Vite preset is primarily for local development. It leaves production `main` to your app config, manifest, or runtime defaults.

### `tanstack-start`

```toml
[tanstack-start]
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]
```

The deploy entry is emitted by the `tako.sh/vite` plugin during `vite build`.

### `nextjs`

```toml
[nextjs]
main = ".next/tako-entry.mjs"
dev = ["next", "dev"]
```

The deploy entry is emitted by the `tako.sh/nextjs` adapter.

Go presets live in `presets/go.toml`. The current Go family manifest is intentionally empty because Go's base runtime defaults come from the Go runtime plugin.

## Choosing A Preset

Use top-level `runtime` to pick the runtime family, then use a runtime-local preset alias:

```toml
runtime = "bun"
preset = "tanstack-start"
```

`runtime` may be `bun`, `node`, or `go`.

Do not namespace the preset in `tako.toml`:

```toml
# Invalid in tako.toml
preset = "js/tanstack-start"
preset = "javascript/tanstack-start"
```

Tako qualifies the runtime-local alias internally from the selected runtime.

## Pinned Presets

Official aliases can be pinned by commit hash:

```toml
runtime = "bun"
preset = "tanstack-start@abc1234"
```

The hash must be 7 to 64 hexadecimal characters. Unpinned aliases are resolved from the `master` branch of the official preset repository.

## Unsupported References

`tako.toml` does not support GitHub preset references or arbitrary repository paths:

```toml
# Invalid
preset = "github:owner/repo/path"
preset = "owner/repo"
```

Keep presets as official runtime-local aliases. Runtime behavior should be configured through runtime plugins and app config, not by pointing a project at a custom preset repo.

## Runtime Overrides

Preset family manifests can include runtime-local dev-command overrides:

```toml
[my-framework]
main = "dist/server/entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[my-framework.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Only `dev` can be overridden in a runtime section. `main`, `assets`, and `name` always come from the base preset section.

The built-in JavaScript manifest uses Bun overrides for Vite-based presets because `bunx --bun` goes through a shim that drops file descriptors above 2, which breaks Tako's fd-4 readiness handshake. The override runs Vite through `bun --bun` directly.

## Resolution And Caching

`tako dev` prefers cached or embedded preset data and fetches from GitHub only when nothing local is available.

`tako deploy` refreshes unpinned official aliases from GitHub on each deploy. If the refresh fails, it falls back to cached content. Branch manifests are cached locally for roughly one hour.

GitHub preset fetches use `GH_TOKEN` when set, then `GITHUB_TOKEN`.

## How Presets Affect Deploy

During deploy, a preset can provide:

- the default runtime `main`
- asset directories to merge into `public/`

Asset roots are the preset `assets` plus top-level `assets` from `tako.toml`, deduplicated and applied in order. Later roots overwrite earlier files.

The final deploy archive still comes from the app build output. Presets do not run builds.

## How Presets Affect Dev

`tako dev` chooses the command in this order:

1. top-level `dev` in `tako.toml`
2. preset `dev`
3. runtime default

Runtime defaults:

- Bun runs the SDK Bun HTTP entrypoint with `{main}`.
- Node runs the SDK Node HTTP entrypoint with `{main}`.
- Go runs `go run .`.

Direct Vite dev commands must use the `tako.sh/vite` plugin so the app writes fd-4 readiness. If no readiness signal arrives and the command looks like Vite, Tako reports a Vite-specific plugin hint.

## Authoring Preset Manifests

The repo's example manifest is `presets/_example.toml`. A family file such as `presets/javascript.toml` contains top-level sections, one per preset alias:

```toml
[my-framework]
main = "dist/server/entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]
```

Base runtime presets such as `bun`, `node`, and `go` can be absent from family files; when no section exists, Tako uses runtime plugin defaults.
