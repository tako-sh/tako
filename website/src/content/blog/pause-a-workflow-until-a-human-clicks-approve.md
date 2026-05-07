---
title: "Pause a Workflow Until a Human Clicks Approve"
date: "2026-04-21T01:27"
description: "A walkthrough of step.waitFor + signal — an order-fulfillment workflow that parks for days waiting on admin approval, then resumes exactly where it left off."
image: ca29555feb14
---

Some workflows can't finish on their own. An order over a certain amount needs a human to eyeball it. A new vendor needs compliance to sign off. A refund above some threshold needs a manager. The work is half-done, the rest depends on a click that might land in two minutes or two days.

The naïve answer is to poll a database column from a cron job. The slightly less naïve answer is to split the workflow into two and wire them together with a webhook. Both are awful — the first burns CPU, the second turns one logical process into three and loses you all your local variables.

Tako's [durable workflow engine](/blog/durable-workflows-are-here) gives you a primitive that's just better: park the run on a named event, sleep the worker, and wake up exactly where you left off when the event fires.

## The setup

Imagine an order-fulfillment workflow. Charge the card, run a fraud check, **wait for an admin to approve high-value orders**, then ship.

```ts
// workflows/fulfill-order.ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow<{ orderId: string }>("fulfill-order", {
  retries: 4,
  handler: async (payload, step) => {
    const order = await step.run("load-order", () => db.orders.find(payload.orderId));

    await step.run("charge", () =>
      stripe.charges.create({ amount: order.total, source: order.token, idempotencyKey: order.id }),
    );

    if (order.total > 50_000) {
      const decision = await step.waitFor<{ approved: boolean; by: string }>(
        `approval:order-${order.id}`,
        { timeout: 7 * 24 * 3600 * 1000 }, // 7 days
      );

      if (decision === null) step.bail("approval timed out — order held");
      if (!decision.approved) step.bail(`rejected by ${decision.by}`);
    }

    await step.run("ship", () => easypost.shipments.create({ to: order.address }));
    await step.run("notify", () => mailer.send(order.email, { orderId: order.id }));
  },
});
```

The interesting line is `step.waitFor`. When the run hits it, the worker doesn't sit and spin — it serializes the run state, marks the row `pending` in the per-app SQLite queue, inserts an `event_waiters` row keyed by the event name, and exits the handler. If nothing else is in flight, the worker subprocess itself shuts down. Zero CPU, zero memory, zero open connections — just a row in a file at `{tako_data_dir}/apps/<app>/runs.db`.

## The signal

Anywhere else in your code — an HTTP handler, a webhook receiver, an admin button — fire the matching signal:

```ts
// app/admin/approve.ts
import { signal } from "tako.sh";

export default async function fetch(req: Request) {
  const { orderId, approverId } = await req.json();

  await signal(`approval:order-${orderId}`, {
    approved: true,
    by: approverId,
  });

  return Response.json({ ok: true });
}
```

The signal lands on tako-server's [internal unix socket](/docs/tako-toml), the matching `event_waiters` row is consumed, the payload is stored as the result of the `waitFor` step, and the run flips back to `pending`. The supervisor wakes the worker, the worker re-claims the run, and execution resumes — `decision` is now `{ approved: true, by: "..." }` and the workflow ships the order.

Notice what _doesn't_ happen on resume: the `load-order` and `charge` steps don't re-run. Their results are already in the `steps` table, keyed by `(run_id, name)`, so on the next claim they return cached values instantly. That's the [`step.run`](/docs/tako-toml) checkpoint contract — at-least-once for the in-flight step, exactly-once for everything before it.

## What "for days" actually means

```d2
direction: right

enq: "POST /orders\n(enqueue run)" {style.fill: "#9BC4B6"; style.font-size: 14}
worker1: "Worker\nclaims, runs steps,\nhits waitFor" {style.fill: "#E88783"; style.font-size: 14}
park: "Run parked\n(row in runs.db)" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 14}
signal: "Admin clicks Approve\n→ signal()" {style.fill: "#9BC4B6"; style.font-size: 14}
worker2: "Worker re-spawns,\nresumes after waitFor,\nships order" {style.fill: "#E88783"; style.font-size: 14}

enq -> worker1
worker1 -> park: "exit"
park -> signal: "...3 days later..."
signal -> worker2: "wake"
```

While the run is parked, your VPS isn't holding anything open for it. The worker process is gone. tako-server can restart, the host can reboot, you can [redeploy](/blog/what-happens-when-you-run-tako-deploy) — the row stays in SQLite, the event waiter stays indexed, and `signal` will still find it three days from now. The 7-day `timeout` is just a safety valve; if it fires first, `waitFor` returns `null` and the workflow takes the cleanup path via `step.bail`.

The same primitive covers webhook callbacks, multi-step onboarding flows that wait on user input, payment-confirmation hops, and anything else where the next step is "the world tells us something happened." One file, one default export, no external queue, no cron polling. Drop it in `workflows/`, run [`tako dev`](/docs/development), and the [embedded scale-to-zero worker](/blog/workflow-workers-scale-to-zero) wires up the rest.
