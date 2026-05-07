---
title: "Multi-Stage Builds for Monorepos"
date: "2026-04-09T04:14"
description: "How Tako's build stages let you deploy monorepo apps with shared packages — no Docker, no CI pipeline, just TOML."
image: 676e890933c4
---

You have a monorepo. A shared UI library in `packages/ui`, an API app in `apps/api`, a web frontend in `apps/web`. Everything shares types, components, maybe a design system. It works great locally.

Then you try to deploy, and the fun stops.

Most deploy tools treat your repo as a single app with a single build command. Monorepos don't work that way. You need to build the shared library first, then the app that depends on it — in the right order, from the right directories. With tools like [Kamal](https://github.com/basecamp/kamal) or [Dokku](https://github.com/dokku/dokku), you end up writing a wrapper script or offloading the whole thing to CI. The deploy tool becomes a dumb uploader.

Tako handles this natively with [`[[build_stages]]`](/docs/tako-toml).

## A real example

Say your monorepo looks like this:

```
packages/ui/        # shared component library
apps/web/           # TanStack Start frontend
  tako.toml
```

Your `apps/web/tako.toml`:

```toml
name = "web"
preset = "tanstack-start"
runtime = "bun"

[[build_stages]]
name = "shared-ui"
cwd = "../packages/ui"
install = "bun install"
run = "bun run build"
exclude = ["**/*.map"]

[[build_stages]]
name = "web-app"
install = "bun install"
run = "vinxi build"
exclude = ["**/*.map", "src/**/*.test.ts"]

[envs.production]
route = "app.example.com"
servers = ["prod"]
```

That's it. Run [`tako deploy`](/docs/deployment) and both stages execute in order — shared library first, then the app. No wrapper script, no Makefile, no CI pipeline glue.

## How it works

```d2
direction: right

stages: Build Stages {
  s1: "shared-ui\n(packages/ui)" {
    style.fill: "#9BC4B6"
  }
  s2: "web-app\n(apps/web)" {
    style.fill: "#E88783"
  }
  s1 -> s2: "in order"
}

artifact: "Deploy\nArtifact" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

server: "tako-server" {
  style.fill: "#2F2A44"
  style.font-color: "#FFF9F4"
}

stages.s2 -> artifact: "exclude\npatterns"
artifact -> server: "SFTP"
```

Each stage runs sequentially in declaration order. For every stage, Tako:

1. Resolves the working directory (`cwd` relative to your app root — `..` is allowed for reaching sibling packages)
2. Runs `install` if specified
3. Runs `run`
4. Collects `exclude` patterns, auto-prefixed with the stage's `cwd`

After all stages complete, Tako packages the artifact — respecting `.gitignore`, force-excluding `.git/`, `.env*`, and `node_modules/` — and ships it to your servers via SFTP. The server runs a production install and starts your app with [zero-downtime rolling updates](/blog/zero-downtime-deploys-without-a-container-in-sight).

The workspace root is guarded: `cwd` can go up with `..` to reach sibling packages, but it can't escape the project root. You get monorepo flexibility without security surprises.

## What about caching?

Tako caches build artifacts locally under `.tako/artifacts/`. The cache key includes a source hash, so if nothing changed, your next deploy skips the build entirely. Cached artifacts are checksum-verified before reuse — corrupted caches are discarded and rebuilt automatically.

This means your second deploy of the same code to a different [environment](/docs/tako-toml) is near-instant. Build once, ship to staging and production from the same artifact.

## Stages vs single build

|                        | `[build]`                  | `[[build_stages]]`                  |
| ---------------------- | -------------------------- | ----------------------------------- |
| **Use case**           | Single app, one build step | Monorepo or multi-step builds       |
| **Working directory**  | One `cwd`                  | Per-stage `cwd` with `..` traversal |
| **Exclude patterns**   | Top-level `exclude`        | Per-stage `exclude` (auto-prefixed) |
| **Install step**       | One `install`              | Per-stage `install`                 |
| **Mutual exclusivity** | Can't combine with stages  | Can't combine with `[build].run`    |

They're mutually exclusive by design. If you have `[[build_stages]]`, don't set `[build].run` — Tako will tell you if you try.

## No Docker, no CI, just deploy

The monorepo deploy problem exists because most tools assume "one repo = one container = one build." Tako doesn't use containers, so it doesn't inherit that assumption. Your build stages run directly on your machine, in order, with full access to your monorepo's dependency graph.

Combined with [presets](/docs/presets) for framework-specific defaults and [multi-server environments](/blog/one-config-many-servers) for routing, you get a deploy workflow that actually fits how modern TypeScript monorepos work — not how Docker wishes they worked.

Check out the [full config reference](/docs/tako-toml) or the [deployment guide](/docs/deployment) to get started.
