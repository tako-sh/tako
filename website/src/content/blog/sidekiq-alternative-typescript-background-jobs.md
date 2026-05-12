---
title: "A Sidekiq Alternative for TypeScript Background Jobs"
date: "2026-05-11T11:31"
description: "Compare Sidekiq-style Redis queues with Tako workflows for durable TypeScript jobs, retries, checkpoints, and scale-to-zero workers."
image: c5d323f04183
---

[Sidekiq](https://github.com/sidekiq/sidekiq) is the default mental model for background jobs: put work on a queue, let workers chew through it, retry failures, keep HTTP fast. It earned that place. For Ruby apps, especially Rails, Sidekiq is still one of the cleanest answers in the category.

But a lot of new apps are not Ruby apps. They are TypeScript apps running on Bun or Node, maybe with a little Next.js, Hono, TanStack Start, or Vite SSR around the edges. When those apps need background work, the usual answer is to rebuild the Sidekiq shape with Redis, a queue library, a worker process, a scheduler, retry rules, a dashboard, and deploy glue.

Tako takes a different path. [Tako workflows](/blog/durable-workflows-are-here) put durable TypeScript background jobs inside the same platform that already deploys your app: per-app queue state, retries, step checkpoints, cron, sleeps, signals, named worker groups, logs, secrets, and workers that can scale to zero on your own VPS.

## Sidekiq is a queue; Tako is app infrastructure

Sidekiq's core design is small and sharp. A Ruby job class defines `perform`, application code calls `perform_async`, and Sidekiq workers pull jobs from a Redis-compatible backend. Its README describes the model plainly: Sidekiq uses threads to handle many jobs concurrently in one process, and modern Sidekiq supports Redis, Valkey, and Dragonfly-compatible backends.

That is a great shape when the rest of the app is already Ruby. The job system fits the language, the framework, the deploy pattern, and the operational habits.

In a TypeScript app, the same shape usually turns into a stack:

| Need             | Sidekiq-style queue stack                                                                       | Tako workflow                                            |
| ---------------- | ----------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| Queue storage    | Redis-compatible service                                                                        | Per-app SQLite at `{tako_data_dir}/apps/<app>/runs.db`   |
| Job definition   | Ruby class with `perform`                                                                       | `workflows/*.ts` with `defineWorkflow`                   |
| Enqueue          | `perform_async`, `perform_in`, `perform_at`                                                     | Typed `.enqueue(payload, opts?)`                         |
| Retries          | [Job retry policy](https://github.com/sidekiq/sidekiq/wiki/Error-Handling), Retry set, Dead set | Run-level and step-level retries, terminal `dead` status |
| Progress         | Job code must persist its own progress, or use iterable jobs for cursor-style work              | `ctx.run` stores named step results automatically        |
| Worker isolation | Separate Sidekiq process, queues, capsules, or more processes                                   | Named worker groups in `tako.toml`                       |
| Cron             | Enterprise periodic jobs or third-party scheduler gems                                          | `schedule` on `defineWorkflow`                           |
| Deployment       | App deploy plus Sidekiq process management plus queue backend                                   | Same `tako deploy` as the HTTP app                       |
| Idle cost        | Worker stays up unless your process manager stops it                                            | `workers = 0` spawns on enqueue and idles out            |

The important difference is ownership. In a Sidekiq-style stack, the queue backend is a shared external component. Your app talks to Redis; worker processes talk to Redis; schedulers talk to Redis; monitoring talks to Redis. In Tako, workflow state belongs to `tako-server`. App code and worker code speak over the internal Unix socket, and the SDK never opens the queue database directly.

```d2
direction: right

sidekiq: "Sidekiq-style TypeScript stack" {
  direction: down
  app: "HTTP app"
  redis: "Redis-compatible queue"
  scheduler: "scheduler"
  worker: "worker process"

  app -> redis: "enqueue"
  scheduler -> redis: "due jobs"
  worker -> redis: "fetch / retry"
}

tako: "Tako workflows" {
  direction: down
  app2: "HTTP app"
  server: "tako-server"
  db: "runs.db"
  worker2: "workflow worker"

  app2 -> server: "unix socket"
  server -> db: "persist runs + steps"
  server -> worker2: "supervise"
  worker2 -> server: "claim / save / complete"
}
```

That does not make one model universally better. It makes the failure modes different. Sidekiq is a queue system you operate beside your app. Tako workflows are part of the app platform. If you already deploy with Tako, the queue is not another thing.

## Durable steps instead of one big job

A classic background job starts simple:

```ts
await sendWelcomeEmail.enqueue({ userId: "u_123" });
```

The hard part is what happens inside the job. Maybe it loads a user, creates an audit row, calls a mail provider, writes a delivery record, and notifies a CRM. If the process dies after the email is sent but before the delivery record is saved, what should retry do?

In a plain queue model, the answer usually lives in your application code. Make the job idempotent. Write progress rows. Use unique database constraints. Store provider idempotency keys. Break large jobs into smaller jobs. Those are still good practices, and Tako does not remove them.

What Tako adds is a workflow-level checkpoint:

```ts
// src/workflows/send-welcome-email.ts
import { defineWorkflow } from "tako.sh";

type Payload = { userId: string };

export default defineWorkflow<Payload>("send-welcome-email", {
  retries: 4,
  handler: async (payload, ctx) => {
    const user = await ctx.run("load-user", () => db.users.find(payload.userId));

    await ctx.run("create-audit-row", () =>
      db.audit.create({
        type: "welcome-email",
        userId: payload.userId,
      }),
    );

    await ctx.run(
      "send-email",
      () =>
        mailer.send({
          to: user.email,
          template: "welcome",
          idempotencyKey: `welcome:${payload.userId}`,
        }),
      { retries: 3, backoff: { base: 2_000, max: 30_000 } },
    );
  },
});
```

Each `ctx.run` result is persisted as a named step. If the worker restarts after `create-audit-row`, the next attempt reads that saved result and resumes at `send-email`. If `send-email` throws, the step can retry locally before the whole workflow consumes a run-level retry.

This is closer to durable workflow engines than to a raw job queue. Sidekiq has its own answer for long-running resumable work with [Iterable Jobs](https://github.com/sidekiq/sidekiq/wiki/Iteration), which store cursor state and resume within a sequence. Tako's checkpointing is different: each workflow can name arbitrary steps in ordinary async code. That is useful when the units are not just "next row in a cursor" but "charge card", "send email", "wait for webhook", "write final status".

The contract stays honest. Tako is still at-least-once. If a worker dies after a side effect succeeds but before the step-save RPC completes, the step can run again. Use idempotency keys, upserts, and stable business identifiers. The difference is that the happy path for durable progress is built into the workflow context, not hand-rolled for every job.

## Workers should have lanes, not traffic jams

Sidekiq has queues, concurrency settings, and newer capsules for controlling execution. Those are real tools. If you run a mature Sidekiq installation, you probably already separate critical jobs from slow jobs so password resets do not sit behind thumbnail generation.

Tako has the same basic need, but the config lives with the app:

```toml
[workflows]
workers = 0
concurrency = 10

[workflows.email]
workers = 1
concurrency = 20

[workflows.media]
workers = 0
concurrency = 4
```

Then assign a workflow to a group:

```ts
export default defineWorkflow("process-image", {
  worker: "media",
  retries: 4,
  handler: async (payload, ctx) => {
    const original = await ctx.run("download", () => storage.get(payload.key));
    const thumb = await ctx.run("resize", () => resize(original));
    await ctx.run("upload", () => storage.put(`thumb/${payload.key}`, thumb));
  },
});
```

The email group can stay warm. The media group can scale to zero until the first image job lands. Each group gets its own worker subprocess, and server-specific overrides can tune the heavy group on a bigger machine. The full precedence rules are in the [`tako.toml` reference](/docs/tako-toml), and the production lifecycle is covered in [deployment docs](/docs/deployment).

This matters because most background job systems eventually become scheduling systems for scarce resources. Some work is latency-sensitive. Some work is CPU-heavy. Some work needs lots of outbound network concurrency. Putting every job into one general worker pool is simple until it is not.

## When Sidekiq is still the right answer

If you run Rails, use Sidekiq. That is the boring, correct answer for a huge number of teams. You get a mature ecosystem, a Web UI, commercial Pro and Enterprise features, and a queue model Ruby developers already understand.

Sidekiq is also the better fit when your background work is part of a larger Ruby system, when Redis-compatible infrastructure is already a standard dependency, or when your team wants Sidekiq's specific operational model.

Tako is for a different moment: you have a TypeScript app, you want Sidekiq-shaped reliability, but you do not want to add a Redis queue stack just to send email, resize images, sync webhooks, run cron, or wait for a human approval. You want the work to deploy with the app, read the same [secrets](/blog/secrets-without-env-files), stream through the same logs, and run on the VPS you already pay for.

The local loop is the same shape too:

```bash
tako dev
```

`tako dev` runs the HTTP app and workflow runtime with the same architecture as production, so enqueues, logs, worker crashes, and code edits behave like the deploy target. The [development docs](/docs/development) go deeper on the local proxy and worker lifecycle; the [CLI reference](/docs/cli) covers deploy, logs, scaling, and rollbacks.

Sidekiq proved that background jobs should feel ordinary. Tako borrows that lesson for TypeScript apps, then pulls the queue into the platform layer: one app, one deploy, one server, durable jobs included.
