---
layout: ../../layouts/DocsLayout.astro
title: "Framework presets for Next.js, TanStack Start, and more - Tako Docs"
heading: Presets
current: presets
description: "Learn how Tako presets provide framework-specific defaults for entrypoints, static assets, and dev commands across supported frameworks."
---

# Presets

Presets are small framework manifests. They give Tako defaults for three things:

- `main`: the runtime entrypoint used after build
- `assets`: static asset directories copied into deployed `public/`
- `dev`: the command used by `tako dev`

Presets do not contain build commands, install commands, or production start commands. Runtime behavior lives in Tako's runtime plugins for Bun, Node, Deno, and Go.

## Choosing a Preset

Use a preset when it matches your framework:

| Framework       | `runtime`                | `preset`         |
| --------------- | ------------------------ | ---------------- |
| TanStack Start  | `bun` or `node`          | `tanstack-start` |
| Next.js         | `bun` or `node`          | `nextjs`         |
| Vite dev server | `bun`, `node`, or `deno` | `vite`           |

Example:

```toml
runtime = "bun"
preset = "tanstack-start"
```

If no framework preset fits, omit `preset` and set `main` yourself:

```toml
runtime = "node"
main = "server/index.mjs"
```

## Built-In Presets

### `tanstack-start`

```toml
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]
```

The entry file is emitted by `tako.sh/vite` during `vite build`.

### `nextjs`

```toml
main = ".next/tako-entry.mjs"
dev = ["next", "dev"]
```

The entry file is emitted by `withTako()` from `tako.sh/nextjs`.

### `vite`

```toml
dev = ["vite", "dev"]
```

This preset is useful for dev-command defaults. It does not set a production `main`.

## Runtime Selection

Presets are runtime-local in `tako.toml`. Choose the runtime with the top-level `runtime` field and keep `preset` as the local alias:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Do not use namespaced preset values in `tako.toml`:

```toml
# Invalid in tako.toml
preset = "js/tanstack-start"
```

Pinned official aliases are supported:

```toml
preset = "tanstack-start@abc1234"
```

`github:` preset references are not supported in `tako.toml`.

## Resolution and Caching

Official preset definitions live in family manifests such as `presets/javascript.toml` and `presets/go.toml`.

`tako dev` prefers embedded or cached preset data so local development starts quickly and works offline when possible. It fetches from GitHub only when no local copy is available.

`tako deploy` refreshes unpinned aliases from the official `master` branch on each deploy. If the refresh fails, it falls back to cached content.

GitHub preset fetches use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.

Fetched manifests are cached locally for about one hour. Pinned aliases use the requested commit when available.

## Runtime Overrides

Preset manifests can define runtime-specific `dev` overrides:

```toml
[vite]
dev = ["vite", "dev"]

[vite.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Only `dev` can be overridden in a runtime subtable. `name`, `main`, and `assets` always come from the base preset section.

## Entrypoint Fallbacks

When Tako needs a runtime entrypoint, it checks:

1. `main` in `tako.toml`
2. manifest main, such as `package.json` `main`
3. preset `main`
4. JavaScript index fallbacks when the preset uses an index-style path

If none of those produce an entrypoint, `tako dev` and `tako deploy` fail with guidance.

## Runtime Plugins

The runtime plugin decides how to install dependencies, launch the app, detect package managers, and resolve runtime downloads.

Current runtimes:

- `bun`
- `node`
- `deno`
- `go`

For JavaScript runtimes, `package_manager` can override detection:

```toml
runtime = "node"
package_manager = "pnpm"
```

If omitted, Tako checks `package.json` `packageManager`, then lockfiles.
