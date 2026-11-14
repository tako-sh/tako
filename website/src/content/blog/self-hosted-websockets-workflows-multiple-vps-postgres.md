---
title: "Self-Hosted WebSockets and Workflows Across Multiple VPS Servers with Postgres"
date: "2026-06-07T00:27"
description: "Use postgres_url to share Tako channel replay and workflow state across multiple VPS servers instead of per-server SQLite."
image: ec7ad3d89cdd
---

> **Update, November 14, 2026:** This article describes an earlier Tako release. Published encrypted secrets now live on network servers, with ignored drafts in each worktree. Native channels, presence, and store notifications can also use SQLite across servers; durable workflow storage has separate requirements. See [current secrets](/docs/secrets/), [native replay](/docs/channels/#deploy-replay-storage), and [workflow storage](/docs/workflows/). Read [the new network and secrets workflow](/blog/sync-deployment-secrets-with-tako-networks/).

Single-server state is easy to reason about. One app, one proxy, one SQLite file, one place where channel replay and workflow runs live.

Multi-server state is where the footguns start. If a user opens a WebSocket connection to the Tokyo VPS and your checkout route publishes from the Los Angeles VPS, that publish still has to arrive. If a scheduled workflow runs on three servers, it should not accidentally send the same reminder three times unless that is exactly what you asked for.

Set the environment credential `postgres_url` to use shared Postgres runtime state for durable channels and workflows. Native Tako networks also support channel fanout and bounded replay across servers with SQLite; shared durable workflow runs still require Postgres unless every workflow is explicitly local.

This is not a new app database abstraction. Your product data still belongs in your app database. This is Tako-owned runtime state: channel replay, workflow runs, workflow steps, event waiters, schedules, and cron coordination. The full config surface lives in [`tako.toml`](/docs/tako-toml/), the deploy checks are covered in [Deployment](/docs/deployment/), and the command lives in the [CLI reference](/docs/cli/).

## The setup

Start with a normal multi-server environment:

```toml
name = "orders"
runtime = "bun"
preset = "nextjs"
app_root = "."

[envs.production]
network = "0123456789abcdef0123456789abcdef"
routes = ["app.example.com"]
servers = ["lax", "nrt", "fra"]
```

Then store the shared runtime database URL as a provider credential:

```bash
tako credentials set postgres_url --env production
```

That is intentionally not a top-level `postgres_url` field in `tako.toml`. Provider credentials are encrypted in `.tako/secrets.json`, scoped to an environment, and sent only through the deployment binding that needs them. They are not exposed to app code, not included in generated secret types, and not pushed by `tako secrets sync`.

With that one credential set, Tako chooses shared storage for the runtime pieces:

| Runtime state     | Single-server default                               | With `postgres_url`                                     |
| ----------------- | --------------------------------------------------- | ------------------------------------------------------- |
| Channel replay    | Local SQLite at `data/tako/channels.sqlite`         | Postgres schema `tako_channels`, keyed by deployed app  |
| Workflow runs     | Local SQLite at `data/tako/workflows.sqlite`        | Postgres schema `tako_workflows`, keyed by deployed app |
| Channel publish   | Store before fanout on the local server             | Store before fanout in shared replay                    |
| Channel reconnect | Replay from the local server's retained rows        | Replay from the shared retained rows                    |
| Workflow cron     | Local schedule set                                  | Shared workflow storage and coordination                |
| SDK access        | SDK talks to `tako-server` over the internal socket | Same SDK path; `tako-server` owns the database writes   |

The deployed app id matters here. Tako scopes runtime state to `{name}/{env}`, not to a release or one process. A rolling deploy can replace instances without making old channel cursors or workflow runs belong to the wrong build.

```d2
direction: right

browser: "Browsers\nWS / SSE"
lax: "LAX VPS\ntako-server"
nrt: "NRT VPS\ntako-server"
fra: "FRA VPS\ntako-server"
pg: "Postgres\nschemas:\ntako_channels\ntako_workflows"
app: "App code\npublish / enqueue"

browser -> lax: "connect"
browser -> nrt: "connect"
app -> fra: "HTTP route publishes"
fra -> pg: "append channel message"
lax -> pg: "poll replay"
nrt -> pg: "poll replay"
lax -> browser: "fanout + replay"
nrt -> browser: "fanout + replay"
app -> lax: "enqueue workflow"
lax -> pg: "insert run + steps"
```

## Why channels need shared replay

Tako channels are durable WebSocket/SSE endpoints under `/_tako/channels/<name>`. A publish is inserted before delivery, and reconnecting clients can replay retained messages from a bounded window. The default replay window is 10 minutes, which is meant for browser reloads, laptop sleep, short network drops, and rolling deploys.

On one server, SQLite is perfect for that. It is local, fast, and private to the app. On multiple servers, local SQLite would split the replay log into islands. A subscriber connected to one server would only see messages that landed on that same server.

Shared Postgres fixes the shape. A publish on any server writes to `tako_channels`; subscribers on every server poll the same replay store, fan out new retained rows, and can reconnect against the same cursor space.

On a native Tako network, the channel broker can instead exchange live events and bounded replay between servers while retaining SQLite locally. Outside that native path, multi-server channel deployment requires `postgres_url`. A configured Postgres credential remains in use and is validated; joining a native network does not silently migrate it. See [channel replay storage](/docs/channels/#deploy-replay-storage) for the current limits.

## Why workflows get a choice

Workflows are different. Many workflows should be global: send one receipt, charge one card, run one daily digest, process one webhook. For those, shared Postgres is the right default in a multi-server environment. The workflow engine stores runs, completed step results, waits, schedules, and leader leases in `tako_workflows`, while workers still run as supervised app-adjacent processes.

But some workflows are intentionally local. A cache warmer that runs once per server is local. A regional health sampler is local. A cleanup task for files on that VPS is local. Those should not need a global database.

For that case, set `local: true` in every workflow that should stay per-server:

```ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow("warm-local-cache", {
  local: true,
  schedule: "*/10 * * * *",
  async handler(payload, ctx) {
    await ctx.run("warm", async () => {
      ctx.logger.info("warming this server");
    });
  },
});
```

The safety rule is simple:

| Project shape                                      | Deploy behavior                                                      |
| -------------------------------------------------- | -------------------------------------------------------------------- |
| One server, channels or workflows                  | SQLite is allowed                                                    |
| Multiple native-network servers, channels           | Native fanout/replay can use SQLite; configured Postgres stays in use |
| Multiple servers without native channel transport  | `postgres_url` is required for channels                              |
| Multiple servers, workflows with no `local: true`  | `postgres_url` is required                                           |
| Multiple servers, every workflow has `local: true` | Per-server SQLite is allowed                                         |
| Native channels plus only local workflows          | Per-server SQLite is allowed                                         |
| Imported schemas with no local workflow definitions | Imports alone do not require workflow storage                       |

Tako checks this before build/deploy work starts. That matters. The failure happens while you are still at the CLI, with an action like:

```bash
tako credentials set postgres_url --env production
```

No half-deployed release, no accidental split-brain runtime, no learning from duplicated emails.

## What changes for app code?

Almost nothing.

Your channel definitions still live in `<app_root>/channels/`:

```ts
import { defineChannel } from "tako.sh";

export default defineChannel("orders", {
  auth: "public",
}).$messageTypes<{
  updated: { orderId: string; status: string };
}>();
```

Your workflows still live in `<app_root>/workflows/`:

```ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow<{ orderId: string }>("send-receipt", {
  retries: 4,
  async handler(payload, ctx) {
    await ctx.run("send", async () => {
      ctx.logger.info("sending receipt", { orderId: payload.orderId });
    });
  },
});
```

And your app still publishes or enqueues through the SDK:

```ts
import orders from "@/channels/orders";
import sendReceipt from "@/workflows/send-receipt";

await orders().publish({
  type: "updated",
  data: { orderId: "ord_123", status: "paid" },
});

await sendReceipt.enqueue({ orderId: "ord_123" });
```

SDKs do not open SQLite or Postgres directly; they talk to `tako-server` over the internal socket, and `tako-server` owns the selected backend.

## Publish or enqueue from a different app

An app in the same network and environment can reach these primitives by the owner's name:

```ts
import { channel, workflow } from "tako.sh";

await channel("orders.orders").publish({
  type: "updated",
  data: { orderId: "ord_123", status: "paid" },
});
const runId = await workflow("orders.send-receipt").enqueue({ orderId: "ord_123" });
```

The first `orders` is the app's `name` in `tako.toml`; the second is its channel name. Run `tako gen` in that app, list `../orders/tako-schema.json` under root-level `schemas` in the caller's config, then generate the caller's types. A staging caller reaches `orders/staging`. The recipient owns publication storage and the workflow's retry policy.

Use `.run(payload)` when the caller needs the workflow's retained result. Inside a JavaScript workflow this becomes a durable child call: the parent checkpoints the child ID and waits without holding its worker slot. For execution without enqueueing, define a function in `network-handlers/` and use `call("orders.lookup-order", input)`. Direct handlers have no automatic retries. See [Network Primitives](/docs/network-primitives/) for app authentication, generated Go/Rust DTOs, and the JavaScript recipient requirement.

Choose shared Postgres where durable runs or shared replay need it, or use native channel replication with local workflow state where that fits the application. Adding servers and adding app-to-app calls are separate decisions from choosing where state persists.

Read the [How Tako Works](/docs/how-tako-works/) runtime section, the [`tako credentials`](/docs/cli/#tako-credentials) command docs, or the [multi-server deployment guide](/docs/deployment/) to wire it up. The app can stay boring. The state is finally shared where it needs to be.
