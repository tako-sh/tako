---
layout: ../../layouts/DocsLayout.astro
title: "Durable Workflows - Tako Docs"
heading: Workflows
current: workflows
description: "Build durable background workflows with typed payloads, checkpointed steps, retries, sleeps, signals, and scale-to-zero execution."
---

# Workflows

Tako workflows run background work next to your app without a separate queue service. A workflow can retry after failures, remember completed steps, sleep without holding a worker open, wait for an external signal, run on a cron schedule, and let its worker process scale to zero when idle.

This guide uses the JavaScript and TypeScript API from `tako.sh`.

## Vocabulary

| Term     | Meaning                                                                         |
| -------- | ------------------------------------------------------------------------------- |
| Workflow | A named handler defined in `<app_root>/workflows/`.                             |
| Run      | One execution of a workflow with its own payload, run ID, attempts, and status. |
| Step     | A named, checkpointed operation inside a run, created with `ctx.run`.           |
| Worker   | A separate process that claims and executes workflow runs.                      |

Run statuses are `pending`, `running`, `succeeded`, `cancelled`, and `dead`. A run may return to `pending` while waiting for a retry, sleep, or signal.

## Define A Workflow

Create one module per workflow under `<app_root>/workflows/` and default-export the handle returned by `defineWorkflow`:

```ts
// src/workflows/fulfill-order.ts
import { defineWorkflow } from "tako.sh";

type FulfillOrderPayload = {
  orderId: string;
};

export default defineWorkflow<FulfillOrderPayload>("fulfill-order", {
  retries: 4,
  handler: async (payload, ctx) => {
    const order = await ctx.run("load-order", () => db.orders.find(payload.orderId));

    await ctx.run("charge", () =>
      payments.charge({
        orderId: order.id,
        amount: order.total,
        idempotencyKey: `${ctx.runId}:charge`,
      }),
    );

    await ctx.run("send-receipt", () => mailer.send(order.email));
  },
});
```

The payload type flows into `.enqueue(payload)`, so callers get normal TypeScript checking without a generated workflow registry. The workflow name must be unique within the app. Using the filename stem as the name keeps discovery and logs easy to follow.

Useful definition options include:

| Option     | Meaning                                                                            |
| ---------- | ---------------------------------------------------------------------------------- |
| `retries`  | Retries after the first run attempt. The default is `2`, for three total attempts. |
| `backoff`  | Run-level retry timing with optional `base` and `max` values in milliseconds.      |
| `worker`   | JavaScript worker group; omitted means the default lane.                           |
| `schedule` | A five-field cron expression such as `"0 9 * * 1-5"`.                              |
| `local`    | Keep the workflow on per-server local storage in a multi-server environment.       |

## Enqueue A Run

Import the workflow handle from server-side code and enqueue a JSON-serializable payload:

```ts
import fulfillOrder from "./workflows/fulfill-order";

const runId = await fulfillOrder.enqueue({ orderId: "ord_123" });
```

You can enqueue from a request handler, another workflow, or a script running with Tako's server runtime. Browser code cannot enqueue directly and throws `TakoError("TAKO_UNAVAILABLE")` if it reaches `.enqueue()`.

The second argument controls one run:

```ts
await fulfillOrder.enqueue(
  { orderId: "ord_123" },
  {
    runAt: new Date(Date.now() + 60_000),
    retries: 6,
    uniqueKey: "fulfill:ord_123",
  },
);
```

| Option      | Meaning                                                                                                    |
| ----------- | ---------------------------------------------------------------------------------------------------------- |
| `runAt`     | Do not make the run eligible before this time. The default is now.                                         |
| `retries`   | Override the workflow's retry count for this run.                                                          |
| `uniqueKey` | Reuse the existing run ID when a non-terminal run with the same key already exists. Terminal runs free it. |

`.enqueue()` returns the run ID. Without a `uniqueKey`, each call creates a new run.

## Durable Steps With `ctx.run`

`ctx.run(name, fn)` saves the function's result. When the workflow is claimed again, a completed step with the same name returns its saved result instead of calling `fn` again. Keep step names stable for the lifetime of a workflow run, and keep returned values JSON serializable.

The durability boundary is the saved result, not the side effect itself. If a worker stops after `fn` completes but before Tako saves the result, that step runs again on the next claim. Step bodies therefore have an at-least-once execution contract. Use provider idempotency keys, upserts, conditional writes, or another domain-specific guard for side effects.

Steps can retry independently before an error reaches the run-level retry policy:

```ts
await ctx.run("call-provider", callProvider, {
  retries: 2,
  backoff: { base: 500, max: 10_000 },
});
```

Step retries default to `0`. Set `retry: false` when an error from that step is permanent and should move the run directly to `dead`.

The workflow context also provides `runId`, `workflowName`, `attempt`, and a workflow-scoped `logger`. Each step callback receives the same run metadata plus `stepName` and a step-scoped logger.

## Durable Sleep

Use a stable name and a duration in milliseconds:

```ts
await ctx.sleep("wait-before-reminder", 24 * 60 * 60 * 1000);
```

Sleeps shorter than 30 seconds run inline. Longer sleeps park the run until its wake time so the worker can process other work or exit. Sleeping does not consume the run's retry budget, and the wake time survives worker and server restarts.

## Wait For A Signal

`ctx.waitFor` parks a run until matching server-side code calls `signal`. An optional timeout resolves the wait to `null` instead:

```ts
const decision = await ctx.waitFor<{ approved: boolean; by: string }>(
  `approval:order-${payload.orderId}`,
  { timeout: 7 * 24 * 60 * 60 * 1000 },
);

if (decision === null) {
  ctx.bail("approval timed out");
}
```

Send the signal from a request handler, webhook, script, or another workflow:

```ts
import { signal } from "tako.sh";

const woken = await signal(`approval:order-${orderId}`, {
  approved: true,
  by: userId,
});
```

`signal` wakes every run waiting on that event name and returns the number of runs woken. Like enqueue, it is server-only and throws `TakoError("TAKO_UNAVAILABLE")` without an installed Tako runtime. Waiting and resuming do not consume retry attempts. Completed steps before the wait remain checkpointed.

## Finish, Bail, Fail, Or Retry

| Handler outcome                   | Run result                                             |
| --------------------------------- | ------------------------------------------------------ |
| Return normally                   | `succeeded`.                                           |
| Throw a regular error             | Retry while attempts remain, then become `dead`.       |
| `ctx.bail(reason?)`               | `cancelled` immediately, with no retry.                |
| `ctx.fail(error)`                 | `dead` immediately, with no retry.                     |
| Long `ctx.sleep` or `ctx.waitFor` | Return to `pending` without spending the retry budget. |

Run retries use exponential backoff with 20% jitter. The default delay starts at one second and is capped at one hour. `retries: 4` means four retries after the first attempt, for five total attempts.

Use `ctx.bail` when the work is no longer needed, such as a rejected or expired approval. Use `ctx.fail` for a permanent error that another attempt cannot fix. To finish successfully before the bottom of a handler, return normally.

Workflow and step lifecycle logs appear in the normal Tako log stream. Use `ctx.logger` and the logger passed to `ctx.run` for messages that carry the run and step context.

## Scale To Zero

Tako keeps no workflow worker running while the queue has no runnable work. Enqueue, cron, a signal, a due retry or sleep, and reclaimed work can wake a worker. The worker exits after its idle window and the next runnable run starts a fresh process.

Production starts one scale-to-zero lane per JavaScript worker group; Go and container workers use a single lane. Workflows with `worker: "email"` run in a process that only claims that group. The config parser also accepts worker counts, concurrency, and per-server overrides; those values are not applied yet.

JavaScript and Go workers honor the runtime-provided lane concurrency (currently 500). Go workers stop claiming on cancellation and wait for active handlers to finish, continuing their lease heartbeats while draining. Concurrency is a worker setting; `defineWorkflow` does not expose per-workflow concurrency or handler timeouts.

The workflow process runs separately from HTTP instances. Heavy workflow dependencies do not have to occupy every request-serving process, while the worker still receives the app's runtime variables, secrets, storage bindings, and structured logs.

If a worker exits with an error before it can claim any work, Tako stops the immediate respawn loop and makes the next enqueue fail with the startup error. Fixing the worker lets the next successful claim restore normal operation.

## Storage And Multiple Servers

On one server, Tako stores durable workflow state locally. Runs belong to the deployed app and environment, not to one worker process or release, so a worker restart or rolling deploy does not discard progress.

Tako keeps finished runs for at least seven days by default, then removes them and their saved steps in small batches. The age is measured from completion, so long sleeps and waits do not shorten a run's history. Cleanup is best effort while the workflow runtime is active; stopped or retired stores can keep older rows until the runtime starts again. To change the age or keep history indefinitely, set `retention` in `tako.toml`:

```toml
[workflows]
retention = "1d" # or "forever"
```

The setting applies to `succeeded`, `cancelled`, and `dead` runs. It does not delete `pending` or `running` runs. SQLite and Postgres can reuse freed database space even when the database file does not immediately shrink.

An environment deployed to multiple servers needs one of two explicit storage models:

| Project shape                                    | Required setup                                                               |
| ------------------------------------------------ | ---------------------------------------------------------------------------- |
| All workflows are global                         | Set the environment's `postgres_url` credential for shared workflow state.   |
| Every JavaScript workflow is intentionally local | Set `local: true` on every workflow to keep separate state on each server.   |
| Local and global workflows are mixed             | Set `postgres_url`; the environment needs shared workflow state.             |
| Go workflow worker on multiple servers           | Set `postgres_url`; Go workflows do not have the source-level local opt-out. |

Configure shared storage with:

```bash
tako credentials set postgres_url --env production
```

`postgres_url` is a provider credential. Tako encrypts it and does not expose it to app code. The SDK API remains the same whether Tako selects local storage or Postgres.

With `local: true`, each server owns its own queue and cron schedule. A scheduled workflow runs once per server, and uniqueness keys do not deduplicate across servers. Use this only for work that is intentionally server-local, such as cleaning local files or warming a regional cache.

Tako validates the multi-server storage choice before build and deploy work begins. See [Deployment](/docs/deployment/) for environment setup and the [CLI reference](/docs/cli/) for provider credential commands.

## Development And Deployment

`tako dev` discovers JavaScript and TypeScript workflows under `<app_root>/workflows/`. The dev daemon owns the durable state and starts a separate scale-to-zero worker when work becomes runnable. Worker output appears in the same log stream as the app. Workflow definition changes refresh the runtime, and a fresh worker picks up the new code on subsequent work.

Cron schedules are synchronized from the current workflow definitions when a worker starts. Removing a schedule removes its registered cron entry on the next worker startup.

`tako deploy` validates the storage model, packages workflow definitions with the app, and installs the workflow runtime for the release. A new release replaces the previous worker runtime after in-flight work drains. Removing the workflows directory in a later release retires the old workflow runtime. Durable runs remain scoped to the app and environment across releases.

Use `tako logs --tail` to follow app and worker output in production. For local runtime details, see [Development](/docs/development/). For release behavior, see [Deployment](/docs/deployment/).

## Practical Rules

- Keep workflow payloads and step results JSON serializable.
- Give every step a stable name. Changing a name creates a different checkpoint for existing runs.
- Make side-effecting step bodies idempotent. `ctx.run` is durable but still at least once at the save boundary.
- Use `uniqueKey` to suppress duplicate non-terminal enqueues for the same business operation.
- Use `ctx.bail` for expected cancellation and `ctx.fail` for permanent failure.
- Keep `signal` and `.enqueue()` in server-side code.
- Use Postgres for global workflows across multiple servers. Use `local: true` only when once per server is intentional.
