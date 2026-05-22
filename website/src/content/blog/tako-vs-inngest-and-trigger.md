---
title: "Tako vs Inngest and Trigger.dev"
date: "2026-04-16T00:47"
description: "Inngest and Trigger.dev gave JavaScript a durable-step model worth copying. Tako ships the same primitives embedded in the server that already runs your app — no separate queue service, no per-run bill."
image: fc35651d63c2
---

Every app eventually grows a background-work problem, and for the past few years the best answer in JavaScript has been <a href="https://www.inngest.com" target="_blank" rel="noopener noreferrer">Inngest</a> or <a href="https://trigger.dev" target="_blank" rel="noopener noreferrer">Trigger.dev</a>. Both teams made durable steps, retries, crons, sleeps, and signals feel ordinary — you write async code, and the platform handles crashes, restarts, and "wait three days for a human to approve." That's a genuinely nice developer experience, and [Tako's new workflow engine](/blog/durable-workflows-are-here) borrows the shape of it on purpose.

The difference is where the engine lives.

## At a glance

|                       | **Inngest**                                 | **Trigger.dev**                             | **Tako**                              |
| --------------------- | ------------------------------------------- | ------------------------------------------- | ------------------------------------- |
| **Deployment model**  | SaaS (self-host available)                  | SaaS (self-host available)                  | Embedded in your app's server         |
| **Durable primitive** | `step.run`, `sleep`, `waitForEvent`         | `retry.onThrow`, `wait.for`, schedules      | `ctx.run`, `ctx.sleep`, `ctx.waitFor` |
| **Queue storage**     | Hosted (self-host: multi-service stack)     | Hosted (self-host: Postgres + Redis + more) | Per-app SQLite file on your VPS       |
| **Billing unit**      | Per execution (~$0.00005/exec on Pro)       | Per-second compute + per-run                | None — runs on a box you already own  |
| **Free tier**         | 50k executions / month                      | $5 monthly credits                          | Unlimited                             |
| **Self-host effort**  | Separate stack (queue, executor, state, UI) | Docker Compose or Helm stack                | Zero — it's already in `tako-server`  |
| **Open source**       | Yes (5.2k stars)                            | Yes, Apache 2.0 (14.6k stars)               | Yes, part of Tako                     |

## What Inngest and Trigger.dev get right

Both companies deserve real credit. Inngest's team basically introduced the "TypeScript-native step functions" category to a generation of devs who'd never heard of Temporal. Trigger.dev pushed the ergonomics further — per-task machine sizing, a beautiful dashboard, Realtime, and, as of v3, a proper open-source license.

The SDK primitives they landed on are the right primitives. Named durable steps for memoized side effects, durable sleep for long waits, event-driven pauses — this vocabulary is now the default way JavaScript developers reason about durable work, and we adopted the same vocabulary in Tako on purpose. If you know one of their SDKs, our [workflows docs](/docs/tako-toml) should feel familiar within ten seconds.

If you want a hosted workflow platform with dashboards, observability tooling, and someone paged when the queue gets backed up at 4am, those are the two names to look at first.

## Where Tako is different

### It's not a service — it's a feature

Inngest and Trigger.dev are workflow platforms. Even self-hosted, you're running _another system_ next to your app: Inngest self-hosting is a multi-service install (event API, runner, executor, state store, dashboard); Trigger.dev's self-host is Postgres + Redis + web + workers wired together with Docker Compose or a Helm chart. That's fine — it's what a full workflow platform requires.

Tako's workflow engine lives inside `tako-server`, the same process that already runs your app's HTTP proxy, TLS, and scale-to-zero supervision. The queue is a single SQLite file at `{tako_data_dir}/apps/<app>/runs.db`. The worker is a subprocess. The protocol is a unix socket. There is no second thing to deploy, monitor, or upgrade — [`tako deploy`](/blog/what-happens-when-you-run-tako-deploy) ships your HTTP handlers and your `workflows/*.ts` files in the same release.

```d2
direction: right

saas: SaaS workflow platform {
  direction: down

  app: Your app
  net: Internet
  queue: Managed queue + dashboard
  worker: Hosted workers

  app -> net: enqueue
  net -> queue: persist
  queue -> worker: dispatch
  worker -> net: results
}

tako: Tako {
  direction: down

  app: Your app
  server: tako-server
  db: runs.db (SQLite)
  worker: Worker subprocess

  app -> server: unix socket
  server -> db: persist
  server -> worker: supervise
}
```

### The billing unit is "you already paid for the box"

Inngest charges per execution — each step counts as one. Trigger.dev charges per-second compute plus a per-run fee. Both are fair pricing for a hosted service, and both scale gracefully for small projects. But it's a per-run meter on every durable step your code executes, forever.

Tako's workflow engine runs on the VPS you were going to pay for anyway. The marginal cost of one more workflow run is the CPU it uses. Enqueue a million a day; the bill doesn't move.

### Same primitives, smaller surface

We kept the API to the handful of things that actually matter: [`ctx.run`, `ctx.sleep`, `ctx.waitFor`, `signal`, and cron via `defineWorkflow`'s `schedule` option](/docs). That's enough to express retries, long waits, human approvals, fan-out, and scheduled jobs. You can read the full contract in [SPEC.md](https://github.com/lilienblum/tako) in one sitting.

## Different ambition

Inngest and Trigger.dev are workflow platforms with big roadmaps — AI agents, realtime, fine-grained observability, managed cloud. If your whole job is background work at scale, a dedicated platform is a reasonable bet.

Tako is building the other direction: [the platform layer between your code and the internet](/blog/durable-channels-built-in), with workflows as one feature among durable channels, [secrets](/blog/secrets-without-env-files), [local dev](/docs/development), deploy, proxy, and TLS. All of it runs from one `tako-server` on one box, with a single config file. No queue vendor, no dashboard login, no per-run meter — just a `ctx` object in your workflow.

If you already love Inngest or Trigger.dev, keep loving them. If "one more vendor for this" is the thing that's been pushing you toward a different approach, [give Tako workflows a try](/docs/cli).
