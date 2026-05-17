---
layout: ../../layouts/DocsLayout.astro
title: "Framework presets for Next.js, TanStack Start, and more - Tako Docs"
heading: Presets
current: presets
description: "Learn how Tako presets provide framework-specific defaults for entrypoints, static assets, and dev commands across supported frameworks."
---

# Presets

Presets are framework manifests. They tell Tako the framework entrypoint, static asset roots, and local dev command for common app stacks.

Presets are intentionally small. Runtime plugins define production install commands, start commands, runtime downloads, package-manager behavior, runtime environment variables, and default build commands.

## Runtime Plugins vs Presets

| Layer          | Owns                                                                                                                                                                             |
| -------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Runtime plugin | Runtime id, language, entrypoint candidates, runtime download, package manager, production install, start command, default build command, default dev command, runtime env vars. |
| Preset         | Framework alias, default deployed `main`, default `assets`, optional dev command override, optional runtime-local dev override.                                                  |
| `tako.toml`    | App choice of `runtime`, `preset`, explicit overrides, routes, vars, builds, storage, source IP, and deploy targets.                                                             |

This split keeps preset files predictable and makes runtime behavior consistent across frameworks.

## Selecting A Preset

Use a runtime-local alias in `tako.toml`:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Do not put the runtime namespace in `tako.toml`:

```toml
# Invalid in tako.toml
preset = "js/tanstack-start"
```

Tako qualifies the preset internally from `runtime`. This keeps app config minimal and avoids mixing runtime selection into the preset string.

You can pin an official preset alias to a commit:

```toml
runtime = "bun"
preset = "tanstack-start@abc1234"
```

Commit pins must be 7 to 64 hexadecimal characters.

## Built-In Preset Manifests

Preset family manifests live under `presets/`:

- `presets/javascript.toml`
- `presets/go.toml`

The current JavaScript manifest includes:

```toml
[vite]
dev = ["vite", "dev"]

[tanstack-start]
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[nextjs]
main = ".next/tako-entry.mjs"
dev = ["next", "dev"]
```

The JavaScript manifest also includes Bun-specific dev overrides for Vite-based presets:

```toml
[tanstack-start.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Those overrides run Vite directly through Bun's ESM loader so fd-4 readiness keeps working.

The Go family manifest is intentionally empty today. Go's base defaults come from the Go runtime plugin:

- `main = "app"`
- dev command `go run .`
- build command `CGO_ENABLED=0 go build -o app .`
- production launch args `{main}`
- no server-side runtime download

## Resolution Rules

When deploying with an unpinned preset alias, Tako fetches the official manifest from the `master` branch and caches it locally. If fetching fails, deploy falls back to cached content when available.

When running `tako dev`, Tako prefers embedded or cached manifest data and only fetches from GitHub if nothing local is available.

Base runtime presets such as `bun`, `node`, and `go` may be absent from a family manifest. In that case, Tako uses the runtime plugin defaults.

## Schema

Each top-level section in a family manifest defines a preset alias:

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

Runtime override sections only support `dev`. Other base preset fields still come from the parent preset section.

## Entrypoint Behavior

Tako resolves the deployed `main` in this order:

1. Top-level `main` in `tako.toml`.
2. Runtime manifest main, such as `package.json` `main`.
3. Preset `main`.

For JavaScript runtimes, when a preset points to `index.<ext>` or `src/index.<ext>`, Tako searches common root and `src/` entrypoint files before using the preset fallback.

If no entrypoint can be resolved, deploy and dev fail with guidance.

## Asset Behavior

Preset `assets` and top-level `assets` in `tako.toml` are combined and deduplicated. Asset directories are merged into the deployed `public/` directory.

For example:

```toml
preset = "tanstack-start"
assets = ["public-extra"]
```

With the built-in TanStack Start preset, deploy includes both `dist/client` and `public-extra`.

## Dev Command Behavior

Tako chooses the dev command in this order:

1. Top-level `dev` in `tako.toml`.
2. Runtime-specific preset override, such as `[vite.bun].dev`.
3. Base preset `dev`.
4. Runtime plugin default.

Runtime defaults are useful for simple apps:

- Bun runs through the `tako.sh` Bun server entrypoint.
- Node runs through the `tako.sh` Node server entrypoint with `--experimental-strip-types`.
- Go runs `go run .`.

Framework presets override those defaults when the framework already provides a dev server.

## Custom Preset Manifests

Preset files are family manifests. The example schema in `presets/_example.toml` shows every supported field:

```toml
[my-framework]
main = "dist/server/entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[my-framework.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Custom preset support is intentionally limited while the protocol is v0. GitHub preset references and `github:` references in `tako.toml` are rejected. Use official aliases and app-level overrides when you need to customize behavior.

## Common Overrides

Override the deployed entrypoint:

```toml
main = "dist/server/custom-entry.mjs"
```

Add static assets:

```toml
assets = ["dist/client", "public"]
```

Override local dev:

```toml
dev = ["pnpm", "dev", "--host", "127.0.0.1"]
```

Override the build itself:

```toml
[build]
run = "pnpm build"
```

These overrides belong in `tako.toml`, not in server config.
