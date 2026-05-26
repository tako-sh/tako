---
title: "The Release Command: Database Migrations During tako deploy"
date: "2026-04-27T13:59"
description: "Tako now runs a one-shot release command on the leader server before the rolling update — the missing primitive for Prisma, Drizzle, and Rails migrations against a shared database."
image: 1136e24f6f83
---

Every team that runs a deploy tool against a real database hits the same wall. You ship code that expects a new column. The rolling update starts. Instance #1 boots on the new schema, instance #2 is still on the old one, and for thirty seconds your two-server cluster is serving requests against two different mental models of the table. Welcome to the migration race.

The fix has always been the same: run the migration **once**, in **one place**, **before** any new instance starts taking traffic. That primitive is now built into Tako.

## The release command

Set `release` in your `tako.toml`:

```toml
name = "my-app"
preset = "tanstack-start"
release = "bun run db:migrate"

[envs.production]
route = "app.example.com"
servers = ["la", "nyc"]
```

That's it. On every [`tako deploy`](/docs/deployment/), after the artifact is extracted and production dependencies are installed, Tako runs `bun run db:migrate` exactly once — on the **leader server** (the first entry in `servers`), inside the freshly-unpacked release directory. Followers wait. If the migration succeeds, the rolling update begins on every server. If it fails, the deploy aborts everywhere, the partial release is cleaned up, and the old instances keep serving traffic on the old schema.

It's a one-line config change for a problem that usually requires a CI pipeline.

## Leader and follower coordination

Here's the sequence when `release` is configured for a multi-server environment:

```d2
direction: down

upload: "Upload + extract artifact\n(parallel, all servers)" {style.fill: "#9BC4B6"; style.font-size: 16}
install: "Production install\n(parallel, all servers)" {style.fill: "#9BC4B6"; style.font-size: 16}
gate: "Release command\n(leader only)" {style.fill: "#E88783"; style.font-size: 16}
followers: "Followers wait\n(blocked at Preparing)" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
result: "Leader publishes result" {style.fill: "#E88783"; style.font-size: 16}
rolling: "Rolling update\n(parallel, all servers)" {style.fill: "#9BC4B6"; style.font-size: 16}

upload -> install
install -> gate
install -> followers
gate -> result
followers -> result: "unblocks"
result -> rolling: "on success"
```

Every server unpacks the artifact and runs `bun install --production` in parallel — that part isn't gated. The new release directory exists on every box. Then the leader runs `sh -c "<release-command>"` once, with cwd set to the new release directory, while followers' `Preparing` task sits on `Waiting for release command`. The leader publishes its exit code, followers unblock, and the rolling update — [zero-downtime, health-checked, drained](/blog/zero-downtime-deploys-without-a-container-in-sight/) — runs on every server in parallel.

Crucially, **no instance on any server starts on the new code** until the migration has succeeded on the leader. That's the whole point: by the time the first new process boots, the schema already matches what the code expects.

## Why one place, not every server

The naive design is "run the migration on every server before that server's rolling update." Don't do this. With a shared database — Postgres, MySQL, anything that lives outside the app servers — you'd have N servers racing each other to add the same column. Even with idempotent migration tools, you'd be paying for transaction-level coordination between servers that have no reason to know about each other.

The leader is just the first server in the env's `servers` list. It has no special permissions, no separate deploy path, no different binary. It's the same `tako-server` process that runs everywhere. It just happens to draw the short straw for one-shot tasks.

| Concern          | How Tako handles it                                                               |
| ---------------- | --------------------------------------------------------------------------------- |
| Where it runs    | First entry in `[envs.<env>].servers`                                             |
| When it runs     | After extract + install, before any rolling update                                |
| What it gets     | Same env as an HTTP instance: vars, secrets, `TAKO_BUILD`, `TAKO_DATA_DIR`, `ENV` |
| Hard timeout     | 10 minutes — process killed and deploy fails                                      |
| On failure       | Deploy aborts on every server, partial release removed, `current` symlink intact  |
| Per-env override | `[envs.<env>].release` overrides top-level; `release = ""` clears it              |

The release command runs with the **same environment an HTTP instance sees at spawn time**: merged `[vars]` + `[vars.<env>]` + decrypted [secrets](/blog/secrets-without-env-files/) + the auto-injected `TAKO_BUILD`, `TAKO_DATA_DIR`, and `ENV`. Your `DATABASE_URL` is just there. No separate config layer for migrations versus app code.

## Per-environment overrides

Real apps usually want different commands per environment. Staging runs migrations against a staging database; production runs them against production; preview environments might skip them entirely. The override is a per-env field:

```toml
release = "bun run db:migrate"   # default for all envs

[envs.production]
route = "app.example.com"
servers = ["la", "nyc"]

[envs.staging]
route = "staging.example.com"
servers = ["staging"]
release = "bun run db:migrate -- --schema staging"   # overrides top-level

[envs.preview]
route = "preview.example.com"
servers = ["preview"]
release = ""   # explicitly clear — preview shares the staging DB, no migration
```

An empty string `release = ""` is meaningful: it clears the inherited top-level value for that environment. Whitespace-only commands are treated the same as unset.

## Beyond migrations

Schema changes are the obvious use case, but the release command is just "run this once before the rolling update." Anything that should happen exactly once per deploy fits:

- **Cache invalidation** — `redis-cli FLUSHDB` against a shared cache so the new code doesn't read stale objects
- **Config push** — upload a generated config blob to a third-party service before instances pick it up
- **Asset upload** — push built static assets to a CDN bucket before instances start serving manifests that reference them
- **Sentry release tagging** — `sentry-cli releases new $TAKO_BUILD` so error tracking lines up with the deploy

The Tako [SDK](/docs/) gives you `TAKO_BUILD` automatically, so your release script knows exactly which version it's preparing the world for.

## What it doesn't do

The release command is intentionally one primitive, not a workflow engine. It runs once per deploy, on one server, with one timeout. If your migration takes longer than 10 minutes, that's a signal to break it into a [workflow](/blog/durable-workflows-are-here/) — durable, resumable, observable — rather than a deploy-time blocker. If you need to coordinate across multiple steps with retries and human approval, [pause-a-workflow](/blog/pause-a-workflow-until-a-human-clicks-approve/) is the better tool.

But for the 95% of deploys where you just need `prisma migrate deploy` or `drizzle-kit push` to run once on the way in — that's now a single line in your config.

Read the [`tako.toml` reference](/docs/tako-toml/) for the full schema, or the [deployment guide](/docs/deployment/) for how it slots into the rest of the pipeline.
