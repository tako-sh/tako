---
title: "How to deduplicate workflow runs with stable IDs"
date: "2026-09-30T15:30"
description: "Use stable run IDs in Tako to deduplicate webhook retries, checkout requests, monthly payouts, and reports. Learn how conflicts and retention work."
image: 61f30323ca01
imageAlt: "An octopus sorts identical envelopes into one outgoing parcel."
---

A webhook arrives twice. Someone double-clicks checkout. Your app queues a payout, but the connection drops before the caller receives the response. The caller retries. Should that create another job?

Tako lets you give a workflow run an ID derived from the operation it represents:

```ts
await fulfillOrder.enqueue({ orderId: "ord_123" }, { id: "fulfill-order:ord_123" });
```

Repeat that call with the same workflow and payload, and Tako returns the existing run ID. That works while the job is waiting, while it is running, and after it finishes, as long as its history remains in storage.

The useful question is what the ID should mean. Let's choose a few.

## One business operation, one run

If you're new to [Tako workflows](/docs/workflows/), each workflow is a named handler in `<app_root>/workflows/`. The default app root is `src`. Its default export gives server-side code a typed `.enqueue()` method. We'll use that method throughout this post.

The second argument accepts `id`. This is the actual run ID, so the value you send is also the string `.enqueue()` returns:

```ts
import fulfillOrder from "./workflows/fulfill-order";

const payload = { orderId: "ord_123" };
const options = { id: "fulfill-order:ord_123" };

const first = await fulfillOrder.enqueue(payload, options);
const second = await fulfillOrder.enqueue(payload, options);

// Both strings are "fulfill-order:ord_123".
```

Tako stores the run before acknowledging enqueue. Concurrent callers supplying this ID create one run. If a response gets lost, the caller can repeat the same request without creating another run.

| Enqueue request                            | Result                              |
| ------------------------------------------ | ----------------------------------- |
| No `id`                                    | Generate an ID and create a new run |
| New `id`                                   | Create a run with that exact ID     |
| Retained ID, same workflow and payload     | Return the existing ID              |
| Retained ID, different workflow or payload | Reject the enqueue with a conflict  |

IDs are unique across workflow names within an app and environment. Include the operation name, such as `fulfill-order:`, so fulfillment and refunds don't accidentally compete for the same order ID. IDs must contain 1–255 bytes and no NUL characters.

Reusing a run preserves its original schedule, retry budget, status, and completed steps. Passing a new `runAt` or `retries` value alongside the existing ID does not edit the run. A successful `.enqueue()` means you have a run ID; it does not mean the work has finished.

```d2
direction: right

request: "Repeated enqueue calls\nsame ID and payload"
store: "Workflow storage" {shape: cylinder}
run: "One retained run"

request -> store: "insert if ID is new"
store -> run: "create once or reuse"
```

## Choose IDs that match your use case

A random UUID generated for every HTTP attempt defeats deduplication. Derive the ID from something that survives retries, or persist a generated operation ID before making the first call. The same intended work needs the same ID; different intended work needs a different one.

| Use case                          | Example ID                          | Meaning                              |
| --------------------------------- | ----------------------------------- | ------------------------------------ |
| Process a webhook event           | `process-event:evt_456`             | One run per event                    |
| Fulfill an order                  | `fulfill-order:ord_123`             | One fulfillment run per order        |
| Pay a vendor for a billing period | `vendor-payout:vendor_42:2026-09`   | One payout run per vendor and period |
| Generate a report for a revision  | `sales-report:team_7:2026-09:rev_3` | One report run for those inputs      |
| Send an onboarding email          | `welcome:user_89`                   | One welcome-email run per user       |

For webhook delivery retries, use the source event's stable identifier. Verify the webhook and select the event you want to process in your request handler, then enqueue from that server-side code:

```ts
import processEvent from "./workflows/process-event";

await processEvent.enqueue({ eventId: event.id }, { id: `process-event:${event.id}` });
```

Two deliveries of one event reuse its run. Two distinct events about one order get separate runs. If your business rule is instead "fulfill this order once," enqueue the fulfillment workflow with the order ID, as in the opening example. Pick the identity of the work rather than the identity of the delivery.

Checkout has the same distinction. Two HTTP requests can refer to one persisted order, so both should derive the fulfillment ID from that order. Generating a request ID inside each handler creates two runs. If your app creates the order during checkout, make order creation idempotent too, so a repeated submission resolves to the same order before enqueue.

Keep the payload stable too. Adding `receivedAt: Date.now()` means a retry sends a different payload and conflicts. Store delivery diagnostics separately. JSON object field order does not affect the comparison, but changed field values do.

Monthly payouts need a period as well as a recipient. Here is a complete workflow definition. `payments` is your application's payment adapter, and its transfer method should pass the idempotency key through to your provider:

```ts
// src/workflows/vendor-payout.ts
import { defineWorkflow } from "tako.sh";
import { payments } from "../payments";

type Payout = {
  vendorId: string;
  period: string;
  amountMinor: number;
  currency: string;
};

export default defineWorkflow<Payout>("vendor-payout", {
  retries: 4,
  handler: async (payload, ctx) => {
    await ctx.run("transfer", () =>
      payments.transfer({
        recipientId: payload.vendorId,
        amountMinor: payload.amountMinor,
        currency: payload.currency,
        idempotencyKey: `${ctx.runId}:transfer`,
      }),
    );
  },
});
```

Enqueue the approved payout from your billing code:

```ts
import vendorPayout from "./workflows/vendor-payout";

await vendorPayout.enqueue(
  {
    vendorId: "vendor_42",
    period: "2026-09",
    amountMinor: 50000,
    currency: "JPY",
  },
  { id: "vendor-payout:vendor_42:2026-09" },
);
```

Get the period and amount from the approved billing record. Recomputing "this month" during each retry can turn a request sent across midnight into two different operations. Agree on your billing timezone and store the period explicitly. A daily digest uses the same approach with a date instead of a month.

If the amount changes from 50000 to 60000 under that same retained ID, enqueue conflicts. Tako does not replace the queued payment. Resolve the changed obligation in your billing records before deciding whether a separate adjustment is needed.

Reports have a different rule. You may want to generate the September report again after correcting its source data. Include the revision in both the ID and payload. Retrying revision 3 reuses revision 3; requesting revision 4 creates another run. Each caller should derive the revision from the same stored report request.

For onboarding, a user ID usually identifies the welcome-email operation. A later product announcement is separate work, so give it its own operation prefix and campaign ID. That lets you suppress repeated welcome requests while still sending intentional future messages to the same user.

For scheduled work, see our [TypeScript cron walkthrough](/blog/self-hosted-cron-jobs-in-typescript-without-redis/). Cron triggers and business operations can have different identities, especially when one scheduled run queues work for many recipients.

## Keep the history and the side effects in mind

Deduplication lasts while Tako retains the run. A repeated enqueue of a `succeeded`, `cancelled`, or `dead` run returns its existing ID without restarting it. If a failed operation needs another attempt, enqueue with a new ID only when you intend to create separate work, after checking what the previous run already did.

Finished runs and their saved steps become eligible for cleanup after 184 days by default. That covers six calendar months, measured from completion. Cleanup happens in bounded batches while the workflow runtime is active. Pending and running runs are never pruned.

You can change this in [`tako.toml`](/docs/tako-toml/#workflows):

```toml
[workflows]
retention = "forever"
```

Use `"forever"` when you want to retain the deduplication record indefinitely. Once cleanup removes a finished run, its ID is available again, and a later enqueue starts fresh without the old checkpoints. Business rules that must outlive workflow history also need durable application records, such as a payout ledger or a unique fulfilled-order record.

Run deduplication protects enqueue. Inside a run, `ctx.run()` saves completed step results, but a worker can stop after a payment succeeds and before the checkpoint reaches storage. The next attempt can execute that step again. This is why the payout example uses `${ctx.runId}:transfer` as a provider idempotency key. Your adapter must implement that protection; the option belongs to the payment integration.

Storage scope matters too. Global workflows across multiple servers use shared Postgres state. Workflows marked `local: true` have separate queues on each server, so their IDs do not deduplicate across servers. The [deployment docs](/docs/deployment/) explain environment setup, and the [workflow storage guide](/docs/workflows/#storage-and-multiple-servers) covers that choice.

Try one workflow through [`tako dev`](/docs/development/). Enqueue identical work twice, check that both calls return your ID, then change the payload and observe the conflict. Start with a business identifier you already have. Add the period or revision when the operation calls for it, keep retries consistent, and let Tako reuse the retained run.
