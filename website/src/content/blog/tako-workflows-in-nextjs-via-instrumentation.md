---
title: "Next.js instrumentation.ts meets initServerRuntime"
date: "2026-04-24T13:48"
description: "Drop a five-line instrumentation.ts into your Next.js app and Tako workflows, signals, and channel publishes light up inside routes and server actions — no ambient globals, no bootstrap glue."
image: 747a4cd2df17
---

Next.js ships a lifecycle hook called [`instrumentation.ts`](https://nextjs.org/docs/app/guides/instrumentation). We just exposed `initServerRuntime()` from `tako.sh/internal`. Snap them together and Tako's durable [workflows](/blog/durable-workflows-are-here/), cross-process [signals](/blog/pause-a-workflow-until-a-human-clicks-approve/), and realtime publishes start working inside your Next.js routes and server actions. Five lines, one file.

## Why Next.js needs a boot hook

Most Tako apps are a single fetch handler — the SDK's runtime entrypoint imports your module and the workflow/channel plumbing is installed in the same process that handles requests. Next.js standalone is structured differently. Our [Next.js adapter](/docs/framework-guides/#nextjs) wraps `next start` and spawns it as a child process, then proxies requests to it. The Tako SDK's boot hook fires in the parent, but your `app/` and `pages/` handlers execute in the child.

```d2
direction: right

parent: tako.sh/nextjs wrapper {style.fill: "#E88783"; style.font-size: 14}
child: next start (your routes) {style.fill: "#9BC4B6"; style.font-size: 14}
runtime: Tako runtime\n(workflows, channels, signals) {shape: cylinder; style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 14}

parent -> child: spawn + proxy
parent -> runtime: boot hook (only here)
child -> runtime: installed by instrumentation.ts
```

Without a boot step on the child side, calling `defineWorkflow(...).enqueue(payload)`, `signal(event, payload)`, or `channel.publish(...)` from inside a route throws `TakoError("TAKO_UNAVAILABLE", "Workflow runtime not installed. ...")`. Everything else — typed [`tako.secrets`](/blog/secrets-without-env-files/), [`tako.env`](/blog/tako-gen-and-the-generated-tako-gen-ts/), and `tako.logger` from `tako.sh` — already works, because those are static imports that don't depend on process-level state.

## What `initServerRuntime()` does

It's the same boot step Tako's plain runtime entrypoint performs, now callable directly. One call per process, idempotent, safe to import in any Node context:

| Step                                          | Effect                                                                        |
| --------------------------------------------- | ----------------------------------------------------------------------------- |
| Install the channel socket publisher          | `channel.publish(...)` can send to subscribers on other instances             |
| Register the workflow runtime                 | `defineWorkflow(...).enqueue()` and `signal()` reach the Tako workflow engine |
| Assert the parent-provided socket env is sane | Fail loud if the child was spawned without Tako's internal env vars           |

It lives on `tako.sh/internal` because it's plumbing — app code never calls it directly.

## Wire it up

Drop this file at the root of your Next.js project, next to `next.config.ts`:

```ts
// instrumentation.ts
export async function register() {
  if (process.env.NEXT_RUNTIME === "nodejs") {
    const { initServerRuntime } = await import("tako.sh/internal");
    initServerRuntime();
  }
}
```

That's it. Next.js calls `register()` once per server process, before any route runs. The `NEXT_RUNTIME === "nodejs"` guard skips the Edge runtime, where `tako.sh/internal` doesn't belong — it reads from a Node fd pipe and speaks over a unix-domain socket.

Now your server code does the obvious thing:

```ts
// app/api/checkout/route.ts
import fulfillOrder from "@/workflows/fulfill-order";
import orderEvents from "@/channels/order-events";

export async function POST(req: Request) {
  const order = await req.json();
  await fulfillOrder.enqueue({ orderId: order.id });
  await orderEvents({ orderId: order.id }).publish({ type: "placed", data: order });
  return Response.json({ ok: true });
}
```

Enqueue a multi-step durable workflow, publish to a live channel, send a `signal()` to a waiting run — all from a standard Next.js route or server action, with typed payloads from the same `defineWorkflow`/`defineChannel` calls you'd use in a plain Tako app.

## The bigger picture

Tako's goal for Next.js is for it to feel like any other fetch handler: [`withTako()`](/docs/framework-guides/#nextjs) in your config, `tako.sh` plus generated [`tako.d.ts`](/blog/tako-gen-and-the-generated-tako-gen-ts/) declarations for typed runtime state and secrets, and now `instrumentation.ts` for the piece the child-process model made awkward. No monkeypatching, no framework globals, no `TakoServer` wrapper object — just Next.js's own lifecycle hook calling one SDK function.

Same backend primitives, same deploy flow, same SDK — [`tako deploy`](/docs/deployment/) still rolls a Next.js app exactly like any other Node/Bun app. The [CLI reference](/docs/cli/) and [framework guides](/docs/framework-guides/) cover the rest.
