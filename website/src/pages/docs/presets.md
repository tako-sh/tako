---
layout: ../../layouts/DocsLayout.astro
title: "Framework Presets - Tako Docs"
heading: Presets
current: presets
description: "Learn how Tako presets provide framework-specific defaults for entrypoints, static assets, and dev commands across supported frameworks."
---

# Presets

Presets are framework manifests. They provide default runtime entrypoints, static asset roots, and development commands so most projects do not need to spell those out in `tako.toml`.

Presets are metadata only. They do not store routes, servers, secrets, storage credentials, SSL policy, build commands, install commands, or production start commands. Runtime plugins own install commands, launch arguments, package-manager behavior, runtime downloads, and base defaults.

## Resolution

Use a runtime-local alias:

```toml
runtime = "bun"
preset = "tanstack-start"
```

Supported built-in JavaScript aliases are:

| Preset           | Defaults                                                                                   |
| ---------------- | ------------------------------------------------------------------------------------------ |
| `vite`           | `dev = ["vite", "dev"]`                                                                    |
| `tanstack-start` | `main = "dist/server/tako-entry.mjs"`, `assets = ["dist/client"]`, `dev = ["vite", "dev"]` |
| `nextjs`         | `main = ".next/tako-entry.mjs"`, `dev = ["next", "dev"]`                                   |

Go currently has a base runtime preset rather than framework-specific entries. Rust apps typically use an explicit native `start` command or container release flow.

Do not use namespaced aliases such as `js/tanstack-start` in `tako.toml`. Choose the runtime with the top-level `runtime` field and keep `preset` runtime-local. `github:` preset references are not supported in project config.

## Runtime Overrides

Preset manifests can declare runtime-local dev-command overrides:

```toml
[tanstack-start]
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[tanstack-start.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Only `dev` can be overridden in nested runtime sections. `name`, `main`, and `assets` always come from the base preset section. Tako uses Bun-specific Vite overrides because `bunx` drops file descriptors above 2, which would break Tako's fd-4 readiness handshake.

## Fetching And Cache

Official preset manifests live in the `tako-sh/presets` GitHub repository and are also embedded in the CLI. Unpinned aliases resolve from the `master` branch on deploy, falling back to cached content when fetching fails. `tako dev` prefers cached or embedded preset data and fetches only when nothing local is available.

Pinned aliases such as `tanstack-start@<commit-hash>` resolve to that commit.

GitHub preset fetches use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.

## How Presets Affect Deploy

Preset `main` is used only when `tako.toml main` and the runtime manifest main field are missing. For JavaScript presets whose `main` is an index-style path, Tako checks existing root and `src/` index files before falling back to the preset path.

Preset `assets` are merged with top-level `assets` and copied into deployed `public/` after build. Later asset roots overwrite earlier ones.

Preset `dev` is used by `tako dev` unless top-level `dev` overrides it. Production build and install still come from `[build]`, `[[build_stages]]`, or the runtime plugin defaults.

## Creating Presets

A family manifest is a TOML file with one top-level section per alias:

```toml
[my-framework]
main = "dist/server/entry.mjs"
assets = ["dist/client"]
dev = ["vite", "dev"]

[my-framework.bun]
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

Fields are optional. Use `main` only when the framework emits a stable server entrypoint. Use `assets` for directories that should be merged into deployed `public/`. Use `dev` when the framework needs a specific development server command.
