---
title: "How to Build Durable Workflows for Bun and Node.js Apps"
date: "2026-06-11T01:37"
description: "Build durable Bun and Node.js workflows with Tako: typed enqueue, retries, step checkpoints, waits, signals, cron, and scale-to-zero workers."
image: f04f3eee79d4
---

Bun and Node.js apps are great at serving requests. They are less great at remembering what happened after the request is gone.

That becomes a problem as soon as one button click turns into five pieces of work: charge the card, write the order, call a fulfillment API, wait for fraud review, send email, retry the flaky webhook, and do none of it twice. A plain `async function` is easy to write, but if the process restarts halfway through, the function forgets everything.

[Tako workflows](/blog/durable-workflows-are-here/) let a Bun or Node app keep that code-shaped workflow while moving progress into durable app infrastructure. You write TypeScript in `src/workflows/`, enqueue from a route handler, and Tako stores runs, completed steps, retries, sleeps, and signals beside the deployed app. No Redis queue, no separate worker platform, no second deploy path.

## Start with a real workflow file

A workflow is a default export from `<app_root>/workflows/<name>.ts`. The default `app_root` is `src`, and the full config surface lives in the [`tako.toml` reference](/docs/tako-toml/).

Here is a checkout workflow with step checkpoints, retry policy, and a human-review pause:

```ts
// src/workflows/fulfill-order.ts
import { defineWorkflow } from "tako.sh";
import { signalFraudTeam } from "../fraud";
import { db } from "../db";
import { mailer } from "../mailer";
import { payments } from "../payments";
import { shipping } from "../shipping";

type Payload = {
  orderId: string;
};

export default defineWorkflow<Payload>("fulfill-order", {
  retries: 4,
  backoff: { base: 5_000, max: 10 * 60_000 },
  handler: async (payload, ctx) => {
    const order = await ctx.run("load-order", () => db.orders.find(payload.orderId));

    const charge = await ctx.run(
      "charge-card",
      () =>
        payments.charge({
          amount: order.total,
          token: order.paymentToken,
          idempotencyKey: `charge:${order.id}`,
        }),
      { retries: 2, backoff: { base: 1_000, max: 30_000 } },
    );

    if (order.total > 50_000) {
      await ctx.run("notify-fraud-team", () => signalFraudTeam(order.id));

      const decision = await ctx.waitFor<{ approved: boolean; by: string }>(
        `fraud-review:${order.id}`,
        { timeout: 3 * 24 * 60 * 60 * 1000 },
      );

      if (decision === null) ctx.bail("fraud review timed out");
      if (!decision.approved) ctx.bail(`fraud review rejected by ${decision.by}`);
    }

    await ctx.run("buy-label", () =>
      shipping.createLabel({
        orderId: order.id,
        address: order.shippingAddress,
        idempotencyKey: `label:${order.id}`,
      }),
    );

    await ctx.run("send-receipt", () =>
      mailer.sendReceipt(order.email, {
        orderId: order.id,
        chargeId: charge.id,
      }),
    );
  },
});
```

The important habit is putting every side effect behind `ctx.run("stable-name", fn)`. Tako stores the returned value for each completed step. If the worker restarts after `charge-card`, the next attempt returns the saved charge result and resumes at the first unfinished step instead of charging again.

That does not remove idempotency from your app. Workflows are still at-least-once: if the process dies after a side effect succeeds but before the checkpoint RPC completes, that side effect can run again. Use provider idempotency keys, upserts, and stable business identifiers. The difference is that you write the durable progress model once, in the workflow, instead of hand-rolling progress rows around every background job.

## Enqueue from a Bun or Node handler

The workflow's default export is also the typed enqueue handle. You import it anywhere server-side code runs under Tako: a fetch handler, Hono route, Next.js route, TanStack Start server function, webhook handler, admin action, or another workflow.

For a plain fetch app:

```ts
// src/index.ts
import fulfillOrder from "./workflows/fulfill-order";

export default {
  async fetch(req: Request) {
    if (req.method !== "POST") {
      return new Response("Method not allowed", { status: 405 });
    }

    const order = await req.json();

    const runId = await fulfillOrder.enqueue(
      { orderId: order.id },
      { uniqueKey: `fulfill:${order.id}` },
    );

    return Response.json({ ok: true, runId });
  },
};
```

`uniqueKey` is the small line that saves you from duplicate POSTs, webhook retries, and impatient double-clicks. If a non-terminal run already has that key, enqueue returns the existing run id instead of inserting another run.

The fraud review signal can come from another handler:

```ts
// src/admin-review.ts
import { signal } from "tako.sh";

export async function approveFraudReview(orderId: string, approverId: string) {
  await signal(`fraud-review:${orderId}`, {
    approved: true,
    by: approverId,
  });
}
```

When the workflow reaches `ctx.waitFor`, the worker does not keep a timer open for three days. Tako parks the run in durable storage, indexes the event name, and lets the worker exit. When `signal()` arrives, the run becomes runnable again and a worker resumes after the wait. The longer walkthrough is in [Pause a Workflow Until a Human Clicks Approve](/blog/pause-a-workflow-until-a-human-clicks-approve/).

The same shape works for delayed work:

```ts
await ctx.sleep("cooldown-before-reminder", 24 * 60 * 60 * 1000);
```

Short sleeps run inline. Longer sleeps defer the run until the wake time, so a one-day wait costs a row, not a process.

## Run it locally, then deploy it

The local loop is deliberately boring:

```bash
tako init
tako dev
```

`tako init` detects the JavaScript runtime and writes the project config. `tako dev` starts the local HTTPS proxy, your HTTP app, and workflow workers using the same environment contract as production. The [development docs](/docs/development/) cover the local `.test` routes and daemon behavior.

For production, the workflow ships with the rest of the app:

```bash
tako deploy
```

Tako discovers `src/workflows/*.ts`, deploys the release, stores workflow state per app, and supervises workers next to HTTP instances. The deployment flow is documented in [Deployment](/docs/deployment/) and the command details are in the [CLI reference](/docs/cli/).

Most apps can start with the default workflow config. If a workflow directory exists and you do not configure workers, Tako treats the app as scale-to-zero: no worker process is kept running until enqueue, signal, cron, delayed retry, sleep wakeup, or lease reclaim makes work runnable.

When you want worker lanes, add named groups:

```toml
[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1
concurrency = 20

[workflows.fulfillment]
workers = 0
concurrency = 4
```

Then assign a workflow:

```ts
export default defineWorkflow<Payload>("fulfill-order", {
  worker: "fulfillment",
  retries: 4,
  handler: async (payload, ctx) => {
    // ...
  },
});
```

This gives latency-sensitive email a warm worker while fulfillment stays scale-to-zero until it has work. The worker lifecycle details are in [Workflow Workers That Scale to Zero](/blog/workflow-workers-scale-to-zero/).

```d2
direction: right

route: "Bun / Node handler" {style.fill: "#9BC4B6"; style.font-size: 14}
socket: "Tako internal socket" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 14}
server: "tako-server" {style.fill: "#E88783"; style.font-size: 14}
db: "workflow storage" {shape: cylinder; style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 14}
worker: "workflow worker" {style.fill: "#E88783"; style.font-size: 14}

route -> socket: ".enqueue() / signal()"
socket -> server: "RPC"
server -> db: "runs, steps, waits"
server -> worker: "spawn or wake"
worker -> server: "claim / save / complete"
```

That division is the reason the SDK stays simple. Your app imports `defineWorkflow`, `.enqueue()`, and `signal()`. Tako owns the queue database, cron ticker, worker supervision, retries, and recovery.

## What to use workflows for

Use a workflow when the work has state you care about after the request ends.

| Need                   | Plain async code            | Tako workflow                      |
| ---------------------- | --------------------------- | ---------------------------------- |
| Retry a flaky API      | Catch and loop in memory    | Run-level and step-level retries   |
| Survive deploys        | Hope the process finishes   | Completed steps are checkpointed   |
| Avoid duplicate starts | Hand-roll a DB lock         | `uniqueKey` on enqueue             |
| Wait for days          | Poll or split the job       | `ctx.sleep` or `ctx.waitFor`       |
| Separate heavy work    | Add another process manager | Named worker groups in `tako.toml` |
| Run scheduled jobs     | Cron plus queue glue        | `schedule` on `defineWorkflow`     |

For simple fire-and-forget work, a direct `await` is fine. For a one-line nightly task, cron might still be enough. But once the job needs retries, checkpoints, human approval, webhook callbacks, or a clean deploy story, durable workflows are the better primitive.

The nice part is that the Bun or Node code still looks like code. A checkout workflow is a TypeScript file, not a YAML state machine. It deploys with the app, reads the same [secrets](/blog/secrets-without-env-files/), logs through the same server, and runs on the same VPS you already picked for HTTP traffic.

Start with one workflow. Put every side effect behind `ctx.run`. Add `uniqueKey` anywhere an enqueue might repeat. Use `ctx.waitFor` when the outside world needs to answer. Then let Tako remember where the work left off.
