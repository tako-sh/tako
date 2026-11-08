---
title: "Durable Workflows Are Here"
date: "2026-04-16T00:29"
description: "Tako now ships a durable workflow engine — step checkpoints, retries, cron, sleep for days, and signal/waitFor — on your own VPS, no external queue service."
image: 1097aa7ec8e5
---

Every app eventually needs background work. Send an email after signup. Reindex a document when it changes. Charge a card, notify a webhook, fan out to three services, wait for a human to approve. That work doesn't belong in the HTTP path — it needs retries, scheduling, and progress that survives the process restarting mid-flight.

The usual answer is another service. Inngest, Temporal, BullMQ on top of Redis, SQS and a Lambda, a cron entry on some random box. One more vendor, one more bill, one more thing to keep alive at 3am.

Tako now ships this as a built-in primitive. A full durable workflow engine runs next to your app — same server, same config, same deploy — and the SDK gives you `ctx.run`, `ctx.sleep`, `ctx.waitFor`, and cron out of the box.

## Step checkpoints that survive crashes

The core idea is `ctx.run` — wrap a side effect, give it a name, and Tako persists its return value. If the worker crashes or restarts, the next attempt skips completed steps and resumes at the first unfinished one:

```ts
// src/workflows/fulfill-order.ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow("fulfill-order", {
  retries: 4,
  handler: async (payload, ctx) => {
    const charge = await ctx.run("charge", () =>
      stripe.charges.create({ amount: payload.total, source: payload.token }),
    );
    const label = await ctx.run("ship", () => easypost.shipments.create({ to: payload.address }));
    await ctx.run("email", () => mailer.send(payload.email, { charge, tracking: label.id }));
  },
});
```

Each run and completed step is stored in the app's workflow database with first-write-wins checkpoint semantics. Retries use exponential backoff with jitter, capped at an hour, and can be overridden per workflow (`retries: 4` means four retries after the first attempt). The execution contract is at least once, so step bodies must be idempotent. See the [Workflows reference](/docs/workflows/) for the full details.

## Return a result to another app

A workflow can return JSON from its handler. For an app named `orders`, a small read workflow could be:

```ts
// orders/src/workflows/order-status.ts
import { defineWorkflow } from "tako.sh";
import { db } from "../db";

export default defineWorkflow("order-status", {
  handler: async (input: { orderId: string }, ctx) => {
    const order = await ctx.run("load", () => db.orders.find(input.orderId));
    return { orderId: order.id, status: order.status };
  },
});
```

`db` is application code. Another managed app imports the generated `orders/tako-schema.json` through root-level `schemas` in `tako.toml`, runs `tako gen`, and calls the workflow by address:

```ts
import { workflow } from "tako.sh";

const status = await workflow("orders.order-status").run({ orderId: "ord_123" });
```

`.run()` enqueues and waits; execution begins when a worker claims the run. `.enqueue()` returns its ID immediately, and `.result(id)` waits for the retained result later. Inside another workflow, `.run()` checkpoints the child ID and terminal outcome, and defers the parent while waiting. Replaying the parent reuses the child; external effects still need idempotency. A waiting timeout does not cancel the child.

For a request/response operation that should execute without enqueueing, define a function in `network-handlers/` and use `call("orders.lookup-order", input)` instead. Direct calls have no automatic retry policy. Both forms stay in the caller's network and environment. The [Network Primitives reference](/docs/network-primitives/) covers generated TypeScript, Go, and Rust types and the current JavaScript recipient requirement.

## Sleep for days, wait for signals

Two primitives turn "workflow" into "long-running business process."

`ctx.sleep("three-days", 3 * 24 * 3600 * 1000)` pauses the run for three days. Short waits run inline; longer ones park the run. The worker exits, the row goes back to `pending` with a wake-up time, and the supervisor resumes on schedule. The wait survives restarts.

`ctx.waitFor(name, { timeout })` parks the run waiting for a named event, then anywhere else in your code, `signal(name, payload)` wakes it:

```ts
// Worker — block the run until approval arrives
export default defineWorkflow("approve-order", {
  handler: async (payload, ctx) => {
    const decision = await ctx.waitFor(`approval:order-${payload.id}`, {
      timeout: 7 * 24 * 3600 * 1000,
    });
    if (decision === null) ctx.bail("approval timed out");
  },
});

// Elsewhere — an HTTP handler, webhook, or another workflow
import { signal } from "tako.sh";
await signal(`approval:order-abc`, { approved: true });
```

Human approvals, webhook callbacks, multi-day onboarding nudges — all expressed as ordinary async code.

## Cron, without the cron box

Pass `schedule` to `defineWorkflow`. Tako runs a leader-elected ticker that enqueues on schedule, deduplicated so a brief outage doesn't double-fire:

```ts
export default defineWorkflow("daily-job", {
  schedule: "0 9 * * *",
  handler: async (payload, ctx) => {
    // daily job body
  },
}); // 9am daily
```

## How it's wired

```d2
direction: right

enq: Enqueue {style.fill: "#9BC4B6"; style.font-size: 16}
server: tako-server {style.fill: "#E88783"; style.font-size: 16}
db: workflows.sqlite {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
worker: Worker process {style.fill: "#E88783"; style.font-size: 16}

enq -> server: "unix socket"
server -> db: "insert run"
server -> worker: "supervise"
worker -> server: "claim / save step / complete"
server -> db: "persist"
```

The worker is a separate process so heavy deps — image libs, ML bindings — don't bloat your HTTP instances. The current production supervisor runs one scale-to-zero lane per app: [same idea as your app](/blog/scale-to-zero-without-containers/), the first enqueue or cron tick spins it up, and an idle worker exits after five minutes. `tako.toml` already parses worker-count, concurrency, and group settings, but production supervision does not consume them yet. See the [current workflow reference](/docs/workflows/) before relying on worker tuning.

## Same server, same deploy

Workflows ship with your app. No external queue to provision, no extra auth tokens, no network hop to a SaaS. Your handlers live in `src/workflows/*.ts` by default, they get [secrets on fd 3](/blog/secrets-without-env-files/) like your HTTP code, they [deploy](/blog/what-happens-when-you-run-tako-deploy/) with everything else, and they keep running across rolling updates.

Run `tako init`, drop a file into `src/workflows/`, `tako dev` boots the worker in-process for unified logs, and `tako deploy` sends the whole thing to your servers. Check [the docs](/docs/tako-toml/) for the full config surface, or the [CLI reference](/docs/cli/) for the commands.

Durable is finally just a keyword.
