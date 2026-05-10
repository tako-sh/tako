---
name: tako-sdk
description: >-
  tako.sh SDK: fetch handler interface, generated tako.gen.ts for runtime state + typed secrets,
  defineChannel/defineWorkflow, Vite and Next.js adapters.
type: framework
library: tako.sh
library_version: "0.0.1"
sources:
  - lilienblum/tako:sdk/javascript/src
---

# Tako SDK (`tako.sh`)

Runtime SDK for JavaScript/TypeScript apps deployed with Tako.

> **CRITICAL**: The `tako.sh` package is **required** — it provides the entrypoint binaries that tako-server launches to run your app. Tako v0 uses plain ES modules everywhere — no `Tako` global. Runtime state (env, secrets, logger, build info) is imported from a generated `tako.gen.ts` file; channels and workflows are imported from their own files.

> **CRITICAL**: Framework helpers are opt-in. Use `tako.sh/vite` for Vite-based SSR frameworks (TanStack Start, Nuxt, SolidStart) and `tako.sh/nextjs` for Next.js standalone builds. Plain fetch-handler apps do not need either helper.

## Core Concept: The Fetch Handler

Tako apps export a standard fetch handler as the default export:

```typescript
// src/index.ts — this is a complete Tako app, no SDK import needed
export default function fetch(request: Request, env: Record<string, string>) {
  return new Response("Hello World!");
}
```

The handler signature is:

```typescript
type FetchHandler = (request: Request, env: Record<string, string>) => Response | Promise<Response>;
```

Two export forms are supported:

```typescript
// Form 1: default export is the fetch function
export default function fetch(req: Request, env: Record<string, string>) {
  return new Response("OK");
}

// Form 2: default export is an object with a fetch method
export default {
  fetch(req: Request, env: Record<string, string>) {
    return new Response("OK");
  },
};
```

## Package Exports

| Import path        | Purpose                                                     | Key exports                                                                                        |
| ------------------ | ----------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `tako.sh`          | Server-side authoring + runtime                             | `defineChannel`, `defineWorkflow`, `signal`, `createImageUrl`, `TakoError`, `InferWorkflowPayload` |
| `tako.sh/client`   | Browser-safe channel client                                 | `Channel`                                                                                          |
| `tako.sh/react`    | React hook for channels                                     | `useChannel`                                                                                       |
| `tako.sh/vite`     | Vite plugin for SSR builds                                  | `tako()` plugin function                                                                           |
| `tako.sh/nextjs`   | Next.js standalone adapter + wrapper                        | `withTako()`, `createNextjsAdapter()`, `createNextjsFetchHandler()`                                |
| `tako.sh/runtime`  | Browser-safe subset consumed by the generated `tako.gen.ts` | `loadSecrets`, `createLogger`, `Logger`                                                            |
| `tako.sh/internal` | Server-only plumbing for framework-adapter boot             | `handleTakoEndpoint`, `initServerRuntime`, channel/workflow define helpers                         |

## Runtime state: `tako.gen.ts`

`tako typegen` emits a project-local `tako.gen.ts` file (inside `src/`, `app/`, or project root - wherever fits the project's tsconfig). It exports typed runtime state and a typed `secrets` bag. When `channels/` or `workflows/` already exists, it also scaffolds empty definition dirs/files so they default-export `defineChannel({ name: "<file-stem>" })` / `defineWorkflow(...)` stubs. Generated channel stubs use the file stem as the initial name, but typegen does not rewrite existing explicit channel names. App code imports what it needs:

```typescript
import { env, isDev, build, port, dataDir, logger, secrets } from "../tako.gen";

export default function fetch(request: Request) {
  logger.info("request", { env, build });
  return new Response(`env=${env} build=${build} db=${secrets.DATABASE_URL ? "ok" : "missing"}`);
}
```

### Surface

| Export    | Description                                                        |
| --------- | ------------------------------------------------------------------ |
| `env`     | `ENV` value (`"development"`, `"production"`, ...)                 |
| `isDev`   | `true` when `env === "development"`                                |
| `isProd`  | `true` when `env === "production"`                                 |
| `port`    | Port assigned to this app instance                                 |
| `host`    | Host/address Tako bound this app instance to                       |
| `build`   | Build identifier (from `TAKO_BUILD`)                               |
| `dataDir` | Persistent app-owned data directory — writes survive restarts      |
| `appDir`  | Directory the app is running from (equivalent to `process.cwd()`)  |
| `secrets` | Typed secret bag (interface regenerated from `.tako/secrets.json`) |
| `logger`  | Structured JSON logger (`logger.info(...)`)                        |

### Secrets

`secrets` is a Proxy that:

- Reads from a mutable store populated via fd 3 at startup (before user module is imported)
- Individual access works: `secrets.MY_KEY` returns the string value
- Resists bulk serialization: `toString()`, `toJSON()` return `"[REDACTED]"`
- Is typed — the `Secrets` interface in `tako.gen.ts` lists every key present in `.tako/secrets.json`

The generated file is server-only. In the browser, use `tako.sh/client` or `tako.sh/react`.

## Images

Server-side JavaScript can create signed optimized image URLs with `createImageUrl`:

```typescript
import { createImageUrl } from "tako.sh";

const url = createImageUrl("/avatars/u_123.png", { width: 256 });
const publicUrl = createImageUrl("/assets/hero.jpg", {
  width: 1200,
  quality: 80,
  public: true,
});
```

The helper uses the app image signing secret from the fd-3 bootstrap. It returns a path under `/_tako/image/v1/...` with no query string. Private URLs are the default and use browser-only caching; pass `public: true` only for non-user-specific images that can be shared by public caches.

## Vite Plugin

For SSR framework builds (TanStack Start, Nuxt, SolidStart, etc.):

```typescript
// vite.config.ts
import { defineConfig } from "vite";
import { tako } from "tako.sh/vite";

export default defineConfig({
  plugins: [tako()],
});
```

**On `vite build`:** Emits `<outDir>/tako-entry.mjs` — a wrapper that normalizes the compiled server module into a default-exported fetch handler. Point `main` in `tako.toml` at this file.

**On `vite dev`:** Adds `.test` to allowed hosts. If `PORT` env var is set, binds Vite to `127.0.0.1:$PORT` with `strictPort: true` (used by `tako dev`).

## Next.js Adapter

For Next.js standalone builds:

```typescript
// next.config.mjs
import { withTako } from "tako.sh/nextjs";

export default withTako({});
```

`withTako()` sets `output = "standalone"` and points `adapterPath` at the Tako adapter shipped in the SDK.

On `next build`, the adapter:

- copies `public/` into `.next/standalone/public/` when standalone output exists
- copies `.next/static/` into `.next/standalone/.next/static/` when standalone output exists
- writes `.next/tako-entry.mjs`

The generated wrapper prefers `.next/standalone/server.js` when it exists. Otherwise it falls back to `next start`.

Point your Tako deploy `main` at `.next/tako-entry.mjs`, or use the `nextjs` preset so that default is provided for you.

### Enabling `.enqueue()` / `signal()` / channel publish inside Next.js routes

The Tako Next.js adapter spawns `next start` as a child process and proxies to it, so the Tako SDK boot hook that runs in the parent process never fires inside your Next.js routes. Add a Next.js `instrumentation.ts` at your project root to install the runtime once per server process:

```typescript
// instrumentation.ts
export async function register() {
  if (process.env.NEXT_RUNTIME === "nodejs") {
    const { initServerRuntime } = await import("tako.sh/internal");
    initServerRuntime();
  }
}
```

After this, server-side routes and server actions can call `defineWorkflow(...).enqueue(payload)`, `signal(event, payload)`, and channel `.publish(...)` normally. Without it, those calls throw `TakoError("TAKO_UNAVAILABLE", "Workflow runtime not installed. ...")`.

## Types

```typescript
import type { FetchHandler, TakoOptions, TakoStatus } from "tako.sh";

// FetchHandler = (request: Request, env: Record<string, string>) => Response | Promise<Response>

// TakoStatus — returned by the internal health endpoint
interface TakoStatus {
  status: "healthy" | "starting" | "draining" | "unhealthy";
  app: string;
  version: string;
  instance_id: string;
  pid: number;
  uptime_seconds: number;
}
```

## Channels

Durable pub-sub streams with SSE and WebSocket transport.

### Defining channels (file-based)

Drop one file per channel in `channels/*.ts` that default-exports `defineChannel({ name: "<name>", ... }).$messageTypes<M>()`. The `name` property is the wire channel name; generated files conventionally use the file stem, but discovery trusts the explicit name and rejects duplicate declared names. Server code imports the file directly to publish.

```typescript
// channels/chat.ts
import { defineChannel } from "tako.sh";

interface ChatMessages {
  msg: { text: string; userId: string };
  typing: { userId: string };
}

export default defineChannel({
  name: "chat",
  paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
  auth: {
    headerName: "authorization",
    async verify(input) {
      // input.params.roomId is typed; operation = "subscribe" | "publish" | "connect"
      const userId = await getUserId(input.header);
      if (!userId) return false;
      return { subject: userId };
    },
  },
  handler: {
    msg: async (data, ctx) => {
      await db.saveMessage(ctx.params.roomId, data);
      return data; // fanned out to subscribers
    },
    typing: async (data) => data,
  },
  replayWindowMs: 24 * 60 * 60 * 1000,
  inactivityTtlMs: 0,
  keepaliveIntervalMs: 25_000,
  maxConnectionLifetimeMs: 2 * 60 * 60 * 1000,
}).$messageTypes<ChatMessages>();
```

- The explicit `name` property is the channel name. `defineChannel({ name: "chat" })` maps to `/_tako/channels/chat`; generated files conventionally use the file stem as the initial name.
- `paramsSchema` serializes to JSON Schema; tako-server validates query params before app auth.
- `.$messageTypes<M>()` is a type-level narrower that declares the message map — runtime no-op. Omit for channels with no typed messages.
- `auth` is optional. Omit or set `false` for public channels.
- `handler` presence decides transport: present → WebSocket, absent → SSE (broadcast-only). SSE channels reject client POST publishes.
- Browser clients reconnect until explicitly closed. Network loss, laptop sleep, server restarts, and clean stream rotation are transient; the SDK retries with bounded backoff, wakes early on the browser `online` event, and resumes from the last received message id while it remains inside the replay window.

Auth return values: `false` deny · `true` allow anonymously · `{ subject }` allow with identity.

### Publishing messages (server-side)

Import the channel module. The export is a typed handle (unparameterized) or a callable taking its params (parameterized). `publish` payloads are type-checked against the declared message map.

```typescript
// Unparameterized channel: direct surface
import status from "../channels/status";
await status.publish({ type: "ping", data: { at: Date.now() } });

// Channel with params: bind params, then publish
import chat from "../channels/chat";
await chat({ roomId: "room1" }).publish({
  type: "msg",
  data: { text: "hello", userId },
});
```

### Subscribing / connecting (client-side)

In the browser, use the `Channel` class from `tako.sh/client` with the declared channel name and optional params. `subscribe()` returns an `EventSource`-shaped subscription; `connect()` returns a `WebSocket`-shaped socket.

```typescript
import { Channel } from "tako.sh/client";

// SSE channel — listen to the raw EventSource
const status = new Channel("status");
const sub = status.subscribe();
(sub.raw as EventSource).addEventListener("message", (e) => {
  const msg = JSON.parse(e.data) as { type: string; data: unknown };
  // ...
});
sub.close();

// WebSocket channel with params
const room = new Channel("chat", "ws", { roomId: "room1" });
const socket = room.connect();
(socket.raw as WebSocket).addEventListener("message", (e) => {
  const msg = JSON.parse(e.data);
  // ...
});
socket.send({ type: "typing", data: { userId: "me" } });

// Publishing from the browser (WS channels only)
await room.publish({ type: "msg", data: { text: "hi", userId: "me" } });
```

For React apps, prefer `useChannel` from `tako.sh/react` — it wraps `Channel` with buffered state, reconnects, and an `onMessage` callback.

### React

`tako.sh/react` exposes a single `useChannel` hook. SSE is the default; pass `transport: "ws"` for WebSocket.

```tsx
import { useChannel } from "tako.sh/react";

function ChatRoom({ room }: { room: string }) {
  const { messages, status, error } = useChannel<{ body: string }>("chat", {
    params: { roomId: room },
  });
  if (error) return <p>error: {error.message}</p>;
  return (
    <ul>
      {messages.map((m) => (
        <li key={m.id}>{m.data.body}</li>
      ))}
    </ul>
  );
}
```

WebSocket with `send`:

```tsx
const { messages, send } = useChannel("chat", {
  params: { roomId: room },
  transport: "ws",
});
```

Return shape (`ChannelConnection<T>`): `messages` (capped at 500, oldest-first), `status` (`"connecting" | "open"`), `error`, `clear()`, and `send(data)` on WebSocket only.

#### Reacting to messages imperatively

Pass an `onMessage` handler when you want to fire a side effect on each incoming message (toast, external store, ref update) without wiring a `useEffect` around the messages array. The hook uses a latest-ref internally, so the handler does not need to be memoized and swapping it does not reconnect:

```tsx
useChannel("notifications", {
  onMessage: (msg) => toast(msg.data.text),
});
```

#### Sharing one connection across components

Each `useChannel` call opens its own SSE/WebSocket. When multiple components in the same tree need the same channel, call the hook once in a provider and fan out via context:

```tsx
const BroadcastCtx = createContext<ChannelConnection<Msg> | null>(null);

export function BroadcastProvider({ children }: { children: React.ReactNode }) {
  const ch = useChannel<Msg>("demo-broadcast");
  return <BroadcastCtx.Provider value={ch}>{children}</BroadcastCtx.Provider>;
}

export function useBroadcast() {
  const ctx = useContext(BroadcastCtx);
  if (!ctx) throw new Error("useBroadcast outside BroadcastProvider");
  return ctx;
}
```

One connection, one buffer, any number of consumers.

### Network routes

| Direction     | Method | Path                     | Transport                        |
| ------------- | ------ | ------------------------ | -------------------------------- |
| Subscribe     | GET    | `/_tako/channels/<name>` | SSE (`text/event-stream`)        |
| Connect       | GET    | `/_tako/channels/<name>` | WebSocket (`Upgrade: websocket`) |
| Publish       | WS     | `/_tako/channels/<name>` | JSON text frame                  |
| Auth callback | POST   | `/channels/authorize`    | Internal (`Host: <app>.tako`)    |

## Workflows

Durable background tasks with retries, schedules, and step checkpointing.

### Authoring workflows

Drop a file in `workflows/<name>.ts` with a default export. The first arg is the workflow name (conventionally the kebab-case file basename), the second is a `WorkflowOpts` object with `handler` plus optional runtime settings:

```typescript
// workflows/send-email.ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow<{ userId: string; to: string }>("send-email", {
  retries: 3, // retries after first attempt (default 2)
  schedule: "0 9 * * *", // cron: daily at 9am (5-field)
  worker: "email", // optional worker group; omitted means "default"
  concurrency: 10, // max parallel runs per worker (default 10)
  timeoutMs: 30_000, // handler timeout (default Infinity)
  backoff: { base: 1_000, max: 3_600_000 }, // exponential backoff
  handler: async (payload, ctx) => {
    ctx.logger.info("send-email started");
    const user = await ctx.run("fetch-user", (step) => {
      step.logger.info("fetching user");
      return db.users.find(payload.userId);
    });
    await ctx.run("send", (step) => {
      step.logger.info("sending email");
      return sendEmail(user, payload.to);
    });
  },
});
```

### Enqueuing

Import the workflow module. The default export is a typed handle with `.enqueue(payload, opts?)` — payload is constrained to the declared `P`.

```typescript
import sendEmail from "../workflows/send-email";

await sendEmail.enqueue({ userId: "u1", to: "a@b.c" });

await sendEmail.enqueue(payload, {
  runAt: new Date(Date.now() + 60_000), // delay
  retries: 9, // override workflow default
  uniqueKey: "digest:2026-04-14", // idempotency: no-op if non-terminal run exists
});
```

No typegen is needed for workflow enqueue typing — the types flow from the workflow module itself.

### Workflow context (`ctx`)

The handler's second argument is the workflow context. Use `ctx` in examples.

| Member                        | Description                                                               |
| ----------------------------- | ------------------------------------------------------------------------- |
| `ctx.run(name, fn, opts?)`    | Memoized step — replays stored result on retry instead of re-executing    |
| `ctx.sleep(name, durationMs)` | Durable sleep — short sleeps inline, long sleeps (≥30s) defer the run     |
| `ctx.waitFor<T>(name, opts?)` | Park until `signal(name)` arrives or timeout; returns `T \| null`         |
| `ctx.bail(reason?)`           | End cleanly as `cancelled` (no retries)                                   |
| `ctx.fail(error)`             | End as `dead` immediately (no retries)                                    |
| `ctx.logger`                  | Workflow-scoped logger                                                    |
| `ctx.runId`                   | The id of the current run                                                 |
| `ctx.workflowName`            | The name of the current workflow                                          |
| `ctx.attempt`                 | The current run attempt number (1-indexed; bumps on each run-level retry) |

`ctx.run` options:

- `retries?: number` — in-step retry attempts (default 0)
- `backoff?: { base?, max? }` — in-step backoff
- `retry: false` — any throw inside `fn` immediately fails the run

The `ctx.run(name, fn, opts?)` callback receives a step context named `step`.

| Member              | Description                                |
| ------------------- | ------------------------------------------ |
| `step.logger`       | Step-scoped logger (`<workflow>:<step>`)   |
| `step.stepName`     | The current step name                      |
| `step.runId`        | The id of the current run                  |
| `step.workflowName` | The name of the current workflow           |
| `step.attempt`      | The current run attempt number (1-indexed) |

`ctx.waitFor` options:

- `timeout?: number` — ms until the step resolves to `null` (default: park indefinitely)

### Signals

```typescript
// Wake all waitFor("approval:order-abc") calls with a payload
import { signal } from "tako.sh";
await signal("approval:order-abc", { approved: true });
```

### Run lifecycle

`pending → running → succeeded | cancelled | dead`

- Throwing a regular error triggers the run-level retry path (exponential backoff).
- `ctx.bail()` → `cancelled`, no retries.
- `ctx.fail()` → `dead`, no retries.

### tako.toml configuration

```toml
[workflows]                # base config inherited by every worker group
workers = 1                # 0 = scale-to-zero (default)
concurrency = 10

[workflows.email]          # named worker-group override
workers = 2

[servers.lax.workflows]    # base override on one server
concurrency = 20

[servers.lax.workflows.email]
workers = 4
```

- `workers = 0` — scale-to-zero: worker spawned on first enqueue/cron tick, exits after 300s idle.
- Precedence for `worker: "email"`: `[servers.<name>.workflows.email]` > `[servers.<name>.workflows]` > `[workflows.email]` > `[workflows]` > defaults.
- If a `workflows/` directory exists but no workflow config exists, the app is implicitly scale-to-zero on every server.

## Common Mistakes

### 1. CRITICAL: Using the Vite plugin for non-SSR apps

```typescript
// WRONG — plain fetch handler app doesn't need the Vite plugin
// vite.config.ts with tako() plugin + src/index.ts with a fetch handler

// CORRECT — the Vite plugin is only for SSR framework builds
// For plain apps, just export a fetch handler and set main in tako.toml
```

### 2. HIGH: Forgetting the Next.js helper for standalone deploys

```typescript
// WRONG — plain Next config without the Tako helper
export default {};

// CORRECT — let Tako configure standalone output and adapterPath
import { withTako } from "tako.sh/nextjs";

export default withTako({});
```

### 3. HIGH: Serializing the secrets object

```typescript
import { secrets } from "../tako.gen";

// WRONG — bulk access is redacted
console.log(JSON.stringify(secrets)); // "[REDACTED]"

// CORRECT — access individual secrets by name
const dbUrl = secrets.DATABASE_URL;
```
