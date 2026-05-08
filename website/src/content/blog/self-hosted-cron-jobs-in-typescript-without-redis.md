---
title: "Self-Hosted Cron Jobs in TypeScript Without Redis"
date: "2026-05-08T02:39"
description: "Build durable TypeScript cron jobs with Tako workflows: scheduled runs, retries, dedupe, step checkpoints, and workers that scale to zero."
image: 5b9490c58c98
---

Cron starts simple. Add `0 9 * * *`, run a script, call it a day.

Then the script sends email, talks to an API, writes to your database, and sometimes fails halfway through. Now you need retries. You need to avoid duplicate sends. You need the job to survive a deploy. You need the worker to wake up at 9am, do the work, then stop burning memory.

The usual TypeScript answer is a queue stack. Redis is excellent for this: streams, sorted sets, and pub/sub make it a natural foundation for job systems. But if your cron job belongs to the same app you already deploy with Tako, adding Redis just to remember "run this every morning, retry safely, and do not double-send" can be more infrastructure than the job itself.

Tako workflows make that a built-in app primitive: TypeScript cron, durable runs, step checkpoints, deduping, and scale-to-zero workers on your own VPS. No separate queue service required.

## A Cron Job Is Really A Queue

A production cron job is not just a clock. The clock is the trigger; the durable queue is what makes the trigger safe.

| Job need         | Redis-backed queue stack                        | Tako workflow                                          |
| ---------------- | ----------------------------------------------- | ------------------------------------------------------ |
| Schedule         | External scheduler or delayed-set polling       | `schedule` on `defineWorkflow`                         |
| Durable state    | Redis persistence or another database           | Per-app SQLite at `{tako_data_dir}/apps/<app>/runs.db` |
| Dedupe           | Job id / uniqueness key in queue library        | `uniqueKey`, plus internal cron keys                   |
| Retries          | Worker library retry policy                     | Run-level and step-level retries                       |
| Worker lifecycle | Separate worker process to deploy and supervise | Tako-supervised worker, scale-to-zero by default       |

Tako's workflow state is owned by `tako-server`, not the SDK. Your HTTP app and worker talk to the shared internal Unix socket; the server inserts runs, stores completed steps, ticks schedules, reclaims expired leases, and wakes workers. The full workflow architecture is documented in [the Tako docs](/docs), and the worker knobs live in [the `tako.toml` reference](/docs/tako-toml).

```d2
direction: right

clock: "cron clock" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
schedules: "schedules table" {style.fill: "#9BC4B6"; style.font-size: 16}
ticker: "tako-server ticker" {style.fill: "#E88783"; style.font-size: 16}
runs: "runs.db" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
supervisor: "worker supervisor" {style.fill: "#9BC4B6"; style.font-size: 16}
worker: "TypeScript workflow" {style.fill: "#E88783"; style.font-size: 16}

clock -> ticker: "every second"
worker -> schedules: "register schedule on boot"
ticker -> runs: "enqueue due run + uniqueKey"
runs -> supervisor: "wake"
supervisor -> worker: "spawn if workers = 0"
worker -> runs: "claim / save steps / complete"
```

That separation matters. The SDK never opens SQLite, so your app code does not carry queue file locking rules around. The server is the one place that knows how to enqueue, claim, heartbeat, persist steps, retry, and recover a run that was stuck in `running` after a worker died.

## Write The Scheduled Workflow

Create a file under `workflows/`. The filename and workflow name match; flat files are discovered by the worker.

```ts
// workflows/daily-digest.ts
import { defineWorkflow } from "tako.sh";
import { db } from "../src/db";
import { mailer } from "../src/mailer";

export default defineWorkflow("daily-digest", {
  // 9:00 UTC every day.
  schedule: "0 9 * * *",
  retries: 4,
  backoff: { base: 10_000, max: 15 * 60_000 },
  handler: async (_payload, step) => {
    const digestDate = await step.run("digest-date", async () =>
      new Date().toISOString().slice(0, 10),
    );

    const targets = await step.run("prepare-targets", async () =>
      db.digestSend.createMissingForDate(digestDate),
    );

    for (const target of targets) {
      await step.run(
        `send:${target.id}`,
        () =>
          mailer.sendDigest(target.email, {
            digestDate,
            idempotencyKey: `digest:${target.id}`,
          }),
        { retries: 3, backoff: { base: 2_000, max: 30_000 } },
      );
    }

    await step.run("mark-complete", () => db.digestRun.markComplete(digestDate));
  },
});
```

The cron run payload is `{}`, so a pure scheduled job usually ignores `_payload`. If you also want to trigger the same workflow manually, import the workflow handle from server-side code and enqueue it yourself:

```ts
import dailyDigest from "../workflows/daily-digest";

await dailyDigest.enqueue(
  {},
  { uniqueKey: `manual-digest:${new Date().toISOString().slice(0, 10)}` },
);
```

That `uniqueKey` is optional for manual runs, but useful when a button, webhook, or admin command might be retried. If another pending or running run already has the same key, enqueue returns the existing run id instead of inserting another row.

Cron runs get the same treatment internally. When the schedule fires, Tako enqueues with a key shaped like `cron:<name>:<bucket_ms>`. If a worker registers schedules twice, or the ticker loops across the same boundary twice, the key collapses the duplicate. If the server falls behind, the ticker fast-forwards and enqueues only the latest boundary that already passed, instead of flooding every missed minute.

## Make Retries Boring

There are two retry layers in the example.

`retries: 4` on the workflow means the whole handler gets up to four retries after the first attempt. If the handler throws, the run goes back to `pending` with exponential backoff and jitter. When the retry budget is exhausted, the run moves to `dead`.

`step.run(..., { retries: 3 })` is smaller. It retries that one side effect before the error escapes to the run-level retry policy. That is useful for a flaky mail API where a quick retry is often enough, while still keeping the whole workflow durable if the process crashes.

The checkpoint is the important part. `step.run("prepare-targets", ...)` stores its result in the `steps` table. On the next attempt, Tako returns the stored result and skips the database write. `step.run("send:<id>", ...)` does the same for each recipient that already finished.

| Practice                             | Why it matters                                                                                              |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------- |
| Use stable step names                | A retried run can find completed work by name.                                                              |
| Make side effects idempotent         | Workflows are at-least-once if a worker dies after the side effect but before the checkpoint RPC completes. |
| Put dedupe in your domain too        | The mailer's `idempotencyKey` or a DB unique key protects the outside world.                                |
| Use `step.fail` for permanent errors | Skip retries when the input can never succeed.                                                              |
| Use `step.bail` for obsolete work    | End cleanly when the job is no longer needed.                                                               |

The contract is honest: durable execution cannot make arbitrary side effects exactly-once. Tako gives you first-write-wins checkpoints, run dedupe, retries, and lease recovery; your step body should still use upserts, idempotency keys, and stable business identifiers.

## Keep The Worker Asleep

For most cron jobs, the best worker is no worker at all until the clock fires.

Scale-to-zero is the default when an app has a `workflows/` directory:

```toml
# tako.toml
name = "digest-app"

[workflows]
workers = 0
concurrency = 10
```

With `workers = 0`, `tako-server` keeps the queue, schedules, and ticker alive. On the first enqueue or cron tick, the supervisor spawns one worker process. The worker claims due runs, processes them up to `concurrency`, and exits after its idle window. In production that idle window is five minutes; under `tako dev` it is shorter so code changes take effect on the next enqueue.

If your job runs constantly, pin workers up:

```toml
[workflows]
workers = 1
concurrency = 20
```

Named groups work too. Put noisy email jobs in `worker: "email"`, then give `[workflows.email]` its own process count. The deployment docs cover the broader release flow in [deployment](/docs/deployment), and the local loop is described in [development](/docs/development).

To try it locally:

```bash
tako dev
```

Edit `workflows/daily-digest.ts`, trigger a manual enqueue from a server route if you want to test immediately, and watch the worker logs in the same terminal stream as the HTTP app. When the scheduled time arrives, the dev server uses the same architecture as production: server-owned queue, internal socket, supervised worker.

To ship it:

```bash
tako deploy production
```

The workflow file deploys with the app. Secrets are available to the worker the same way they are available to HTTP instances, and the worker process is separate, so workflow-only dependencies do not bloat request handling. The CLI surface is in [the CLI reference](/docs/cli).

Self-hosted cron should feel like application code, not like a small distributed systems project you accidentally adopted. Put the schedule next to the handler, name the steps that matter, make side effects idempotent, and let Tako keep the clock, queue, retries, dedupe, and sleeping worker lifecycle together on your VPS.
