---
name: tako-sdk-js
description: >-
  tako.sh SDK: fetch handler interface, tako runtime object, generated tako.d.ts type augmentation,
  defineChannel/definePresence/defineStore/defineWorkflow, live data and offline reads, Vite and Next.js adapters.
type: framework
library: tako.sh
sources:
  - tako-sh/tako:sdk/javascript/src
---

# Tako SDK (`tako.sh`)

Runtime SDK for JavaScript/TypeScript apps deployed with Tako.

> **CRITICAL**: The `tako.sh` package is **required** — it provides the entrypoint binaries that tako-server launches to run your app. Tako v0 uses plain ES modules everywhere — no `Tako` global. Runtime state (env, secrets, logger, build info) is accessed through the `tako` object from `tako.sh`; channels and workflows are imported from their own files.

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

| Import path        | Purpose                                         | Key exports                                                                                                          |
| ------------------ | ----------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `tako.sh`          | Isomorphic runtime + authoring helpers          | `tako`, `imageUrl`, `imageSrcSet`, `defineChannel`, `definePresence`, `defineStore`, `defineWorkflow`, runtime types |
| `tako.sh/server`   | Server-only runtime re-export                   | `tako`, `TakoRuntime`                                                                                                |
| `tako.sh/client`   | Browser-safe realtime clients                   | `Channel`, `configureChannels`, `Store`, `clearStoreCache`                                                           |
| `tako.sh/react`    | React hooks for realtime data                   | `useChannel`, `usePresence`, `useStore`                                                                              |
| `tako.sh/vite`     | Vite plugin for SSR builds                      | `tako()` plugin function                                                                                             |
| `tako.sh/nextjs`   | Next.js standalone adapter + wrapper            | `withTako()`, `createNextjsAdapter()`, `createNextjsFetchHandler()`                                                  |
| `tako.sh/runtime`  | Browser-safe runtime internals                  | `loadSecrets`, `createLogger`, `Logger`                                                                              |
| `tako.sh/internal` | Server-only plumbing for framework-adapter boot | `handleTakoEndpoint`, `initServerRuntime`, channel/workflow helpers                                                  |

## Runtime state: `tako.sh` + `tako.d.ts`

`tako generate` emits a project-local `tako.d.ts` that augments `tako.sh` with typed environment names, typed `tako.secrets` keys, typed `tako.storages` names, channel metadata, store metadata, workflow metadata, and user-defined env vars on `process.env` / `import.meta.env`. Tako-owned runtime values are typed through `tako.sh` exports such as `tako.env`, `tako.build`, and `tako.dataDir`, not through generated env-var globals. It keeps an existing declaration in `app/`, `src/`, or the project root; otherwise it writes to the first existing `app/` or `src/` directory, falling back to the project root. `tako gen` and `tako g` are aliases. When `<app_root>/channels/` or `<app_root>/workflows/` already exists, it also scaffolds empty definition dirs/files so they default-export `defineChannel("<file-stem>")` or `defineWorkflow(...)` stubs. Generated channel stubs use the file stem as the initial name, but `tako generate` does not rewrite existing explicit names. Generated `TakoChannels` entries use the declared name as the key and `import("tako.sh").InferChannel<typeof import("./channels/<file>").default>` for params, messages, and transport metadata. App code imports `tako` from `tako.sh` for project runtime state:

```typescript
import { tako } from "tako.sh";

export default function fetch(request: Request) {
  tako.logger.info("request", { env: tako.env, build: tako.build });
  return new Response(
    `env=${tako.env} build=${tako.build} db=${tako.secrets.DATABASE_URL ? "ok" : "missing"}`,
  );
}
```

### Surface

| Export          | Description                                                                        |
| --------------- | ---------------------------------------------------------------------------------- |
| `tako`          | Frozen runtime object with env, ports, paths, logger, secrets, storages, and cache |
| `tako.env`      | `ENV` value (`"development"`, `"production"`, ...)                                 |
| `tako.isDev`    | `true` when `tako.env === "development"`                                           |
| `tako.isProd`   | `true` when `tako.env === "production"`                                            |
| `tako.port`     | Port assigned to this app instance                                                 |
| `tako.host`     | Host/address Tako bound this app instance to                                       |
| `tako.build`    | Build identifier (from `TAKO_BUILD`)                                               |
| `tako.dataDir`  | Persistent app-owned data directory — writes survive restarts                      |
| `tako.appDir`   | Directory the app is running from (equivalent to `process.cwd()`)                  |
| `tako.secrets`  | Typed secret bag (interface regenerated from `.tako/secrets.json`)                 |
| `tako.storages` | Typed storage bag (interface regenerated from `tako.toml`)                         |
| `tako.cache`    | SQLite-backed server-side key/value cache                                          |
| `tako.logger`   | Structured JSON logger (`tako.logger.info(...)`)                                   |
| `Env`           | TypeScript union of configured environment names                                   |
| `TakoSecrets`   | TypeScript interface of secret names                                               |
| `TakoStorages`  | TypeScript interface of storage names                                              |
| `TakoRuntime`   | TypeScript type of the exported `tako` object                                      |

### Secrets

`tako.secrets` is a Proxy that:

- Reads from a mutable store populated via fd 3 at startup (before user module is imported)
- Individual access works: `tako.secrets.MY_KEY` returns the string value
- Resists bulk serialization: `toString()`, `toJSON()` return `"[REDACTED]"`
- Is typed — the `TakoSecrets` interface augmentation in `tako.d.ts` lists every key present in `.tako/secrets.json`

The generated declaration file is type-only and is not imported by app code. In the browser, use `tako.sh/client` for realtime clients, or `tako.sh/react` for hooks.

## Cache

Server-side JavaScript can cache JSON-serializable values with explicit get/put/delete calls:

```typescript
import { tako } from "tako.sh";

const profile = await tako.cache.get<Profile>(`profile:${userId}`);
if (!profile) {
  const fresh = await fetchProfile(userId);
  await tako.cache.put(`profile:${userId}`, fresh, { ttl: 60_000 });
}

await tako.cache.delete(`profile:${userId}`);
```

`get(key)` returns `undefined` for missing or expired keys. `put(key, value, { ttl })` stores a JSON-serializable value; `ttl` is milliseconds. `delete(key)` removes one key. Native dev and deploy runtimes store cache entries in Tako-managed local SQLite outside app backups. Container releases do not expose `TAKO_DATA_DIR` in v0, so the cache helper is unavailable there.

## Images

JavaScript can create public optimized image URLs with `imageUrl`:

```typescript
import { imageUrl } from "tako.sh";

const photo = imageUrl("/photos/p_123.jpg");
const url = imageUrl("/avatars/u_123.png", { width: 640 });
const avifUrl = imageUrl("https://cdn.example.com/uploads/avatars/u_123.png", {
  width: 640,
  format: "avif",
});
```

The helper returns `/_tako/image?src=...&w=...` and is synchronous. Explicit URL quality values must also match `[images].qualities` (default `[75]`); SDK validation alone does not make a variant server-allowed. Width must be one of `320, 640, 960, 1200, 1920`; quality is `1..100`; format is `webp` or `avif`. Omit `format` for the optimizer default; Tako defaults to WebP, and `format: "avif"` is only valid when the app's `[images].formats` allows AVIF. Local public paths are available by default. Remote URLs must match `[images].remote_patterns` in `tako.toml`. Sources may be JPEG, PNG, GIF, WebP, or AVIF; animated GIF and WebP sources keep animation for optimized resize and crop URLs when emitted as WebP. AVIF output is available for still images; animated sources that request AVIF fall back to WebP. The server verifies the app's configured image guardrails before fetching or transforming.

Use `imageSrcSet` for responsive public images. It returns `{ src, srcSet, sizes }`
for an `<img>` element:

```tsx
import { imageSrcSet } from "tako.sh";

const hero = imageSrcSet("/images/hero.jpg", {
  width: 1200,
  layout: "constrained",
});

export function Hero() {
  return <img {...hero} alt="Application overview" />;
}
```

Layouts are `constrained` (the default), `full-width`, and `fixed`. Override
`widths` and `sizes` for custom responsive markup; each width must be supported
by the helper and allowed by the app's image configuration.

## Storage

Attach S3-compatible storage with the CLI, then use `tako.storages.<name>` from server-side app code:

```bash
tako storages add uploads \
  --provider s3 \
  --bucket app-uploads \
  --endpoint https://<account>.r2.cloudflarestorage.com \
  --region auto
```

```typescript
import { tako } from "tako.sh";

const downloadUrl = await tako.storages.uploads.createDownloadUrl("receipts/r_123.png", {
  expiresInSeconds: 3600,
});

const uploadUrl = await tako.storages.uploads.createUploadUrl("avatars/u_123.png", {
  contentType: "image/png",
});
```

Storage URLs are private by default. S3-compatible bindings use SigV4; local
bindings use signed app-local GET/PUT routes under `/_tako/storages/<binding>/<key>`.
`createImageUrl(key, { public: true, width })` uses the storage `public_base_url`
and the public image optimizer. `createImageSrcSet(key, { public: true, width,
layout })` returns responsive public image attributes. Private storage image
transforms are not implemented yet; use `createDownloadUrl` for private direct
object URLs.

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

**On `vite dev`:** Adds `.test` / `.tako.test` to allowed hosts and binds to `127.0.0.1`. The bound port is reported to `tako dev` over fd 4.

## Next.js Adapter

For Next.js standalone builds:

```typescript
// next.config.mjs
import { withTako } from "tako.sh/nextjs";

export default withTako({});
```

`withTako()` sets `output = "standalone"`, points `adapterPath` at the Tako adapter,
and adds `*.test` / `*.tako.test` to `allowedDevOrigins`. It configures `next/image`
to use Tako's public image optimizer globally, aligning generated widths with
Tako's defaults. Remote image sources must be permitted by `[images].remote_patterns`.

On `next build`, the adapter:

- copies `public/` into `.next/standalone/public/` when standalone output exists
- copies `.next/static/` into `.next/standalone/.next/static/` when standalone output exists
- writes `.next/tako-entry.mjs`

The generated wrapper prefers `.next/standalone/server.js` when it exists. Otherwise it falls back to `next start`.

Point your Tako deploy `main` at `.next/tako-entry.mjs`, or use the `nextjs` preset so that default is provided for you.

### Enabling runtime operations inside Next.js routes

The Tako Next.js adapter runs the standalone server (or the `next start` fallback)
in a child process and proxies to it. Runtime hooks installed in the parent aren't
installed in that child. Add a Next.js `instrumentation.ts` at your project root
to install the runtime once per server process:

```typescript
// instrumentation.ts
export async function register() {
  if (process.env.NEXT_RUNTIME === "nodejs") {
    const { initServerRuntime } = await import("tako.sh/internal");
    initServerRuntime();
  }
}
```

After this, server-side routes and server actions can call workflow `.enqueue(...)`,
`signal(...)`, channel `.publish(...)`, and Store `.invalidate(...)`. Store
invalidation uses the channel publisher, so it needs the same initialization.
Without these hooks, workflow calls report an unavailable runtime and channel or
Store publication can fail. Verify the write and its resulting reader update
through the Tako-provided URL.

## Types

```typescript
import type { FetchHandler, TakoStatus } from "tako.sh/internal";

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

Durable pub-sub routes with SSE and WebSocket transport, plus a bounded replay window. Channels are broadcast streams, not work queues: all authorized subscribers to the same channel read the same messages, and each subscriber keeps its own cursor. Production stores replay by deployed app id (`{name}/{env}`); local dev keeps replay in memory for the current daemon process.

### Defining channels (file-based)

Drop one file per channel in `<app_root>/channels/*.ts` that default-exports `defineChannel("<name>", ...).$messageTypes<M>()`. The first argument is the wire channel name; generated files conventionally use the file stem, but discovery trusts the explicit name and rejects duplicate declared names. Server code imports the file directly to publish.

```typescript
// <app_root>/channels/chat.ts
import { defineChannel } from "tako.sh";

interface ChatMessages {
  msg: { text: string; userId: string };
  typing: { userId: string };
}

export default defineChannel("chat", {
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
  transport: "ws",
  replayWindowMs: 24 * 60 * 60 * 1000,
  inactivityTtlMs: 0,
  keepaliveIntervalMs: 25_000,
  maxConnectionLifetimeMs: 2 * 60 * 60 * 1000,
}).$messageTypes<ChatMessages>();
```

- The first argument is the channel name. `defineChannel("chat")` maps to `/_tako/channels/chat`; generated files conventionally use the file stem as the initial name.
- `paramsSchema` serializes to JSON Schema; tako-server validates query params before app auth. Params do not partition stored messages or fanout: all authorized `roomId` bindings above receive the same `chat` stream. Never use params as a room or tenant isolation boundary.
- `.$messageTypes<M>()` is a type-level narrower that declares the message map — runtime no-op. Omit for channels with no typed messages.
- `auth` is optional. Omit or set `false` for public channels.
- `transport: "ws"` enables WebSocket; the default is receive-only SSE. WebSocket frames are stored and broadcast as sent. Handle application mutations in your app's HTTP endpoints.
- Every publish is stored before delivery. Messages are not claimed or removed when one subscriber receives them. `replayWindowMs` defaults to 10 minutes and can be overridden per channel.
- SSE subscriptions reconnect until closed. React `useChannel` reconnects WebSockets with the latest replay cursor. Lower-level `Channel.connect()` returns a single socket, so non-React callers must implement WebSocket reconnection.

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
const token = await getToken();
const sub = status.subscribe({ authorization: token });
(sub.raw as EventSource).addEventListener("message", (e) => {
  const msg = JSON.parse(e.data) as { type: string; data: unknown };
  // ...
});
sub.close();

// WebSocket channel with params
const room = new Channel("chat", "ws", { roomId: "room1" });
const socket = room.connect({ authorization: token });
(socket.raw as WebSocket).addEventListener("message", (e) => {
  const msg = JSON.parse(e.data);
  // ...
});
socket.send({ type: "typing", data: { userId: "me" } });
```

`authorization` is optional. Omit it for public or cookie-auth channels; use `headers.Authorization` when the app expects a custom authorization value.

For React apps, prefer `useChannel` from `tako.sh/react` — it wraps `Channel` with buffered state, reconnects, and an `onMessage` callback.

### Presence

Define presence spaces in `<app_root>/presence/*.ts` with `definePresence` from `tako.sh`. Auth has the same `headerName`, `cookieName`, `paramsSchema`, and `verify` pattern as channels. Return `false` to deny, `true` for anonymous access, or `{ subject: user.id }` for a verified identity. Omit `auth` for public access.

```ts
// src/presence/lobby.ts
import { definePresence } from "tako.sh";
import { verifyToken } from "../lib/auth";

export default definePresence("lobby", {
  auth: {
    async verify({ header }) {
      const user = await verifyToken(header?.value);
      return user ? { subject: user.id } : false;
    },
  },
});
```

```tsx
import { usePresence } from "tako.sh/react";

const { members, connectionId, status, error, update } = usePresence("lobby", {
  authorization: token,
  data: { typing: false },
});
// In an event handler:
update({ typing: true });
```

`members` contains `{ connectionId, subject?, data }`, including the current connection. Every hook call creates one connection; multiple tabs belonging to the same subject remain separate. Only the server controls identity; member data is untrusted client-provided JSON (up to 16 KiB). `data` supplies initial metadata; `update()` replaces it. The hook waits for authorization, sends heartbeats every 15 seconds, reconnects with current metadata, clears stale state on disconnect, and leaves on unmount. Authorization failures stop retries until connection options change. Membership converges within approximately one second. Connections without updates expire after 45 seconds, so a reconnect can briefly overlap its old connection. Presence has no durable replay. Validated params scope membership: different `chatId` values have separate member lists. Auth must verify access to the requested chat. Channel and presence names have separate namespaces.

Single-server production uses SQLite; multi-server deployments require the shared `postgres_url` credential. Local dev uses an in-memory store and permits connections without running auth callbacks, as with channels. Use channels for durable events and presence for current membership.

### Stores

Use Stores for live reads from an existing database, rather than managing
record snapshots as channel events. Put one default-exported `defineStore`
definition per `<app_root>/stores/*.ts` file:

```ts
// src/stores/messages.ts
import { defineStore } from "tako.sh";
import { listMessages } from "../lib/db";
import { authorizeChat } from "../lib/auth";

export default defineStore("messages", {
  paramsSchema: (t) => t.Object({ chatId: t.String() }),
  auth: {
    async verify({ header, params }) {
      const user = await authorizeChat(header?.value, params.chatId);
      return user ? { subject: user.id } : false;
    },
  },
  load: ({ params, subject, signal }) => listMessages(params.chatId, { subject, signal }),
  offline: { maxAgeMs: 3_600_000, version: "1" },
});
```

The helpers above belong to the app. Loader results use JSON serialization;
`Jsonify` infers the serialized client shape, including dates as strings and
custom `toJSON` results. BigInts, cycles, and non-finite numbers fail the read.
Use enumerable data properties or `toJSON` for custom classes.
`auth.verify` receives `{ store, params, header?,
cookie? }` and returns `false`, `true`, or `{ subject }`; omit `auth` for public
reads. Loader `subject` is verified, never supplied by client params. Schema and
auth apply independently to every read and subscription, including local dev.
Use `headerName: false` plus `cookieName` for cookie-only auth.

```tsx
import { useStore } from "tako.sh/react";

const { data, status, isStale, error, refresh } = useStore("messages", {
  params: { chatId },
  authorization: token,
  cacheScope: userId,
  cacheVersion: "1",
});
```

Run `tako generate` to infer params and data through `TakoStores` and
`InferStore`; do not import the server definition into a client component.
`status` is `loading`, `ready`, `offline`, or `error`; `data` is initially
undefined, and `isStale` exposes cached or disconnected results. `enabled: false`
pauses reads. Each hook owns a reader; share through a provider if needed.
Vanilla clients use `Store` from `tako.sh/client` with `subscribe`, `getSnapshot`,
and `refresh`.

After a committed write, import the definition server-side and call
`await messages.invalidate({ chatId })`. This affects only the exact validated
params scope; an empty scope is not a wildcard. Snapshots load separately per
reader and never broadcast. Reconnects reload current data; invalidations during
loads trigger another read. External writers must also invalidate; Tako does
not perform SQL dependency tracking or CDC. Publication failures reject without
rolling back committed writes. Use a transactional app outbox for durable
commit-and-notify.

Offline persistence needs both backend `offline` and a stable, non-secret client
`cacheScope` for the user/tenant. Never use a bearer token as a scope. Default
IndexedDB storage is bounded (64 entries, 16 MiB total, 1 MiB per entry), and TTL
is capped at seven days. Both cache versions default to `"1"`; bump backend
`offline.version` and client `cacheVersion` together after incompatible shape
changes. Credentials are never persisted. On logout, call
`await clearStoreCache(scope)` from `tako.sh/client`, then disable or unmount
readers; scope clearing also stops active matching readers and notifies other
tabs when available. An authorization denial clears active and persisted data.
Offline clients cannot learn about remote revocation; only persist appropriate
data. The app shell must already be available: no PWA/service worker, offline
write queue, or conflict-resolution engine is installed.

Store names follow `^[A-Za-z0-9][A-Za-z0-9._-]{0,255}$`; params are capped at
16 KiB and complete read responses at 1 MiB. Multi-server subscriptions require
shared `postgres_url`. See https://tako.sh/docs/stores/ for native backends and
the complete lifecycle. Verify scoped invalidation, reconnection, user switching,
and offline expiry, not just the initial render.

### React

`tako.sh/react` exposes `useChannel`, `usePresence`, and `useStore`. For channels, SSE is the default; pass `transport: "ws"` for WebSocket.

```tsx
import { useChannel } from "tako.sh/react";

function ChatRoom({ room, token }: { room: string; token?: string }) {
  const { messages, status, error } = useChannel<{ body: string }>("chat", {
    params: { roomId: room },
    authorization: token,
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

Drop a file in `<app_root>/workflows/<name>.ts` with a default export. The first arg is the workflow name (conventionally the kebab-case file basename), the second is a `WorkflowOpts` object with `handler` plus optional runtime settings:

```typescript
// <app_root>/workflows/send-email.ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow<{ userId: string; to: string }>("send-email", {
  retries: 3, // retries after first attempt (default 2)
  schedule: "0 9 * * *", // cron: daily at 9am (5-field)
  worker: "email", // worker group; omitted means "default"
  local: true, // optional per-server local storage/cron for multi-server deploys
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

Use `local: true` only when per-server local queues and cron are intentional. Multi-server workflows require the encrypted `postgres_url` credential unless every JavaScript workflow is local. Multi-server channels always require `postgres_url` so all servers share publish and replay state.

### Enqueuing

Import the workflow module. The default export is a typed handle with `.enqueue(payload, opts?)` — payload is constrained to the declared `P`.

```typescript
import sendEmail from "../workflows/send-email";

await sendEmail.enqueue({ userId: "u1", to: "a@b.c" });

await sendEmail.enqueue(payload, {
  runAt: new Date(Date.now() + 60_000), // delay
  retries: 9, // override workflow default
  id: "digest:2026-04-14", // the actual run ID
});
```

Omit `id` to generate a new run ID. A supplied ID reuses the retained run in any status when the workflow and payload match. A different workflow or payload conflicts. IDs are unique within the app and environment, must contain 1–255 bytes with no NUL characters, and remain reserved until history is deleted. Finished history is retained for 184 days by default; `[workflows].retention` can override it or use `"forever"`.

No generated file is needed for workflow enqueue typing — the types flow from the workflow module itself.

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

Production starts one scale-to-zero process per JavaScript worker group, selected by `worker: "name"`. Each process receives runtime concurrency (currently 500) and exits after 300 seconds idle. Worker counts, concurrency, and per-server overrides in `tako.toml` are parsed but not yet applied by production supervision.

```toml
[workflows]                # parsed desired base settings
workers = 1                # not applied by production supervision yet
concurrency = 10

[workflows.email]          # parsed desired group override
workers = 2

[servers.lax.workflows]    # parsed desired server override
concurrency = 20

[servers.lax.workflows.email]
workers = 4
```

- The current production lane starts when runnable work appears and exits after 300 seconds idle.
- The precedence model is parsed and tested, but it is not connected to production worker supervision.

## Common Mistakes

### 1. CRITICAL: Adding Vite to a plain fetch-handler app

```typescript
// WRONG — plain fetch handler app doesn't need the Vite plugin
// vite.config.ts with tako() plugin + src/index.ts with a fetch handler

// CORRECT — omit Vite for a plain fetch-handler app; Vite dev servers need the plugin for readiness
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
import { tako } from "tako.sh";

// WRONG — bulk access is redacted
console.log(JSON.stringify(tako.secrets)); // "[REDACTED]"

// CORRECT — access individual secrets by name
const dbUrl = tako.secrets.DATABASE_URL;
```
