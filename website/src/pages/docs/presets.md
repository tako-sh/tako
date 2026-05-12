---
layout: ../../layouts/DocsLayout.astro
title: "Framework presets for Next.js, TanStack Start, and more - Tako Docs"
heading: Presets
current: presets
description: "Learn how Tako presets provide framework-specific defaults for entrypoints, static assets, and dev commands across supported frameworks."
---

# Presets

Presets are small framework manifests. They help Tako choose the runtime entrypoint, static asset roots, and local dev command for common frameworks.

They do not define production start commands, runtime downloads, package-manager install commands, or build behavior. Those belong to runtime plugins.

## What A Preset Can Set

| Field    | Meaning                                                   |
| -------- | --------------------------------------------------------- |
| `name`   | Optional display/name override. Defaults to section name. |
| `main`   | Runtime entrypoint after build.                           |
| `assets` | Static asset directories copied into deployed `public/`.  |
| `dev`    | Command used by `tako dev`.                               |

Example:

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

The Vite preset mainly affects local development. It leaves production `main` to runtime defaults or your app config.

### `tanstack-start`

```toml
[tanstack-start]
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]
```

The deploy entry is emitted by `tako.sh/vite` during build.

### `nextjs`

```toml
[nextjs]
main = ".next/tako-entry.mjs"
dev = ["next", "dev"]
```

The deploy entry is emitted by `tako.sh/nextjs`.

Go presets live in `presets/go.toml`. The current Go family manifest is intentionally empty; Go's base runtime defaults are supplied by the runtime plugin.

## Choosing A Preset

Use a runtime-local alias:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Use `runtime` to choose Bun, Node, or Go. Do not namespace the preset in `tako.toml`:

```toml
# Do not use this in tako.toml
preset = "javascript/tanstack-start"
```

Pinned aliases are allowed:

```toml
preset = "tanstack-start@abc1234"
```

`github:` preset references are not supported in `tako.toml`.

## Resolution

`tako init` loads preset names for the selected runtime family and offers them in the setup flow.

`tako dev` prefers cached or embedded preset data. It fetches from GitHub only when no local data is available.

`tako deploy` refreshes unpinned official aliases from the `master` branch on each deploy. If fetch fails, it falls back to cached content.

Fetched manifests are cached locally for about one hour. GitHub fetches use `GH_TOKEN` when set, then `GITHUB_TOKEN`.

## Runtime Overrides

A preset can override the `dev` command for a specific runtime:

```toml
[vite]
dev = ["vite", "dev"]

[vite.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Only `dev` can be overridden in runtime-local nested sections. `main`, `assets`, and `name` always come from the base preset section.

The built-in Bun overrides avoid `bunx` and `bun x` because those shims drop file descriptors above 2, which would break Tako's fd-4 readiness handshake.

## How Presets Interact With `main`

Tako resolves the deploy/dev entrypoint in this order:

1. top-level `main` in `tako.toml`
2. manifest main such as `package.json` `main`
3. preset `main`
4. runtime default

For JS runtimes, common index files are checked when a preset points at an index-style entry:

- `index.ts`
- `index.tsx`
- `index.js`
- `index.jsx`
- `src/index.ts`
- `src/index.tsx`
- `src/index.js`
- `src/index.jsx`

If no `main` can be resolved, deploy/dev fail with guidance.

## Assets

Asset roots are:

1. preset `assets`
2. top-level `assets` in `tako.toml`

The combined list is deduplicated, then merged into app `public/` after build. Later roots overwrite earlier files.

## Base Runtime Presets

Using only a runtime is valid:

```toml
runtime = "node"
```

Base runtime behavior comes from the runtime plugin:

- Bun defaults to a JS SDK server entrypoint.
- Node defaults to a JS SDK server entrypoint.
- Go builds a binary and runs it directly.

Use a framework preset only when the framework needs a specific deploy wrapper, asset root, or dev command.

## Preset File Format

Official family manifests are TOML files:

```toml
[preset-name]
name = "preset-name"
main = "dist/server/entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[preset-name.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Unknown fields are ignored with warnings. Runtime override sections must be named after a supported runtime id such as `bun`, `node`, or `go`.
