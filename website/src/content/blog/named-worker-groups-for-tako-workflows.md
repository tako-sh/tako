---
title: "Named Worker Groups for Tako Workflows"
date: "2026-04-29T04:29"
description: "Tako workflows now support named worker pools, so a slow image job in the media group can't starve auth-critical email or default work."
image: eef4025ddeaa
---

Workflow queues have one classic failure mode: a slow job clogs the pipe and everything else waits behind it. A 30-second image resize lands in the queue, every worker grabs one, and the password-reset email that should have gone out in 200ms sits in `pending` while your users refresh their inbox.

That's head-of-line blocking. The fix is the same one every queue ends up shipping eventually: separate pools for separate kinds of work. As of today, [Tako workflows](/blog/durable-workflows-are-here) have it built in.

## Pools, named after what they do

You assign a workflow to a named pool with one option:

```ts
// src/workflows/process-image.ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow<{ key: string }>("process-image", {
  worker: "media",
  retries: 4,
  handler: async (payload, ctx) => {
    const buf = await ctx.run("download", () => s3.get(payload.key));
    await ctx.run("resize", () => sharp(buf).resize(1024).toBuffer());
    await ctx.run("upload", () => s3.put(`thumb/${payload.key}`, buf));
  },
});
```

Workflows without `worker:` belong to the `default` group, so existing apps keep working unchanged. Add `worker: "email"` to your transactional sender, `worker: "media"` to anything CPU-heavy, and the runtime takes care of routing each enqueue to the right pool.

## Sized independently in `tako.toml`

Each named group is its own row in the config, with the same two knobs as the base block — `workers` (always-on processes) and `concurrency` (parallel runs per worker). The base `[workflows]` block sets defaults that named groups inherit and override:

```toml
[workflows]
workers = 0          # scale-to-zero default for everything
concurrency = 10

[workflows.email]
workers = 1          # one always-on worker for fast, light jobs
concurrency = 20     # plenty of parallelism per worker

[workflows.media]
workers = 2          # two workers for heavy, CPU-bound jobs
concurrency = 4      # but keep per-worker fan-out low

[servers.lax.workflows.media]
workers = 4          # bump it up on the box that has more cores
```

The precedence chain reads top-down — built-in defaults, then `[workflows]`, then `[workflows.<group>]`, then any `[servers.<name>.workflows.<group>]` override on a specific host. The full table is in [`tako.toml`](/docs/tako-toml).

## Why isolation matters

Without separate pools, every worker is a generalist. One image job lands, every worker grabs an image job, and the queue depth for `send-email` climbs while CPU is pinned by `sharp`. Your auth-critical work is _correct_ — it'll run eventually — but "eventually" is the wrong SLA for a password reset.

With named groups, the runtime spawns a separate subprocess per group, each loading only the workflows assigned to it. The email worker picks up `send-email` runs and ignores `process-image` entirely; the media worker does the inverse. They contend for CPU at the OS scheduler, not at the queue.

```d2
direction: right

ent1: "enqueue send-email" {style.fill: "#9BC4B6"; style.font-size: 14}
ent2: "enqueue process-image" {style.fill: "#9BC4B6"; style.font-size: 14}
server: "tako-server" {style.fill: "#E88783"; style.font-size: 14}
email: "email worker\n(workers = 1)" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 14}
media: "media worker\n(workers = 2)" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 14}
def: "default worker\n(scale-to-zero)" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 14}

ent1 -> server -> email
ent2 -> server -> media
server -> def: "everything else"
```

Each pool keeps its own [scale-to-zero](/blog/scale-to-zero-without-containers) lifecycle: a group with `workers = 0` doesn't spawn until the first matching enqueue or cron tick lands, and idles back out when there's nothing to do. So the `media` group can sit at zero overnight and your VPS doesn't pay rent on it; the `email` group can stay warm because cold-starting an image library every 200ms email isn't free.

## Per-server tuning

The same precedence rules cascade into per-server blocks. If your `lax` box has more cores than your `cdg` box, give `media` four workers there and one elsewhere — same `tako.toml`, [different defaults per host](/blog/one-config-many-servers), no fork in the workflow code.

Drop `worker: "name"` into your handlers, add a `[workflows.<name>]` block to `tako.toml`, and `tako deploy`. The slow jobs get their own lane, the fast jobs stay fast, and your password resets stop waiting in line behind a thumbnail render.
