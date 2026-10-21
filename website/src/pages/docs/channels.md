---
layout: ../../layouts/DocsLayout.astro
title: Channels
seoTitle: "Channels - Tako Docs"
heading: Channels
current: channels
description: "Define typed SSE and WebSocket channels with authorization, replay, browser clients, server-side publishing, and multi-server Postgres storage."
---

Tako Channels are durable broadcast streams for live app events. The Tako proxy serves each channel over SSE or WebSocket, stores every published message before fanout, and retains a bounded replay window for reconnecting clients.

## Choose Broadcast Semantics

Channels are pub-sub, not work queues. Every authorized subscriber to a channel reads the same messages and keeps an independent replay cursor. A message is not claimed, acknowledged, or removed when one subscriber receives it.

Use a channel for live dashboards, chat delivery, presence updates, notifications, and collaborative updates. Use a workflow when work should be claimed and executed with retries instead of broadcast to every subscriber.

Every delivered message has this wire shape:

```ts
type ChannelMessage<T> = {
  id: string; // server-assigned replay cursor
  channel: string; // exact wire channel name
  type: string; // application-defined message type
  data: T;
};
```

## Define A Channel

Put one default export in each `<app_root>/channels/*.ts` file. The first argument to `defineChannel` is the explicit wire name and is the source of truth for the public route; it does not have to match the file name. Use `.$messageTypes<M>()` to declare the payload for each message type without changing runtime behavior.

```ts
// <app_root>/channels/site-events.ts
import { defineChannel } from "tako.sh";
import { readSession } from "../lib/auth";

type SiteMessages = {
  notification: { id: string; text: string };
  presence: { online: number };
};

export default defineChannel("site-events", {
  auth: {
    async verify({ header }) {
      const session = await readSession(header);
      if (!session) return false;
      return { subject: session.userId };
    },
  },
  transport: "ws",
}).$messageTypes<SiteMessages>();
```

## Understand Channel Params

`paramsSchema` is a TypeBox schema for typed query parameters. Tako validates the values before authorization and passes them to `auth.verify`.

Params do **not** change the stored channel name or partition replay and fanout. Every authorized parameter binding for one wire channel receives the same stream. Use params only as connection context when every authorized binding may receive every event. Do not use `roomId`, `tenantId`, or similar params as an isolation boundary.

For example, a shared announcements stream can reject unsupported client versions without creating one stream per version:

```ts
export default defineChannel("announcements", {
  paramsSchema: (t) =>
    t.Object({
      clientVersion: t.String({ minLength: 1 }),
    }),
  auth: {
    verify({ params }) {
      return supportsAnnouncements(params.clientVersion);
    },
  },
});
```

Omit `paramsSchema` for an unparameterized channel. Parameterized exports are callable; unparameterized exports are already bound handles:

```ts
import announcements from "../channels/announcements";
import siteEvents from "../channels/site-events";

const currentAnnouncements = announcements({ clientVersion: "2.4.0" });
const subscription = currentAnnouncements.subscribe();

await siteEvents.publish({ type: "presence", data: { online: 42 } });
```

## Authorize And Choose A Transport

Omit `auth` or set it to `false` for a public channel. Otherwise, `auth.verify` receives the validated `params`, exact `channel`, requested `operation`, and any configured `header` or `cookie`. Production subscribe and connect requests pass `"subscribe"` or `"connect"` as the operation. The callback must return:

- `false` to deny access.
- `true` to allow access without an identity.
- `{ subject: "user-id" }` to allow access with a stable identity.

`headerName` defaults to `authorization`. Set `headerName: false` with `cookieName` for cookie-only auth. Browser clients can pass `authorization: token`; Tako sends `Authorization: Bearer <token>` for SSE and the equivalent auth envelope for WebSocket.

The `transport` option chooses the live transport:

| Need                       | Definition                                          | Browser API            |
| -------------------------- | --------------------------------------------------- | ---------------------- |
| Server-to-browser updates  | Omit `transport`; the channel uses receive-only SSE | `subscribe()`          |
| Browser-to-server messages | Set `transport: "ws"`                               | `connect()` / `send()` |

WebSocket client frames are `{ type, data }`; the proxy stores and broadcasts them as sent. Channels do not run application callbacks for incoming messages.

## Publish From The Server

Import the channel definition in server-side code, bind any params, and publish a typed event:

```ts
import siteEvents from "../channels/site-events";

const published = await siteEvents.publish({
  type: "presence",
  data: { online: 42 },
});
```

`publish()` requires the Tako server runtime and returns the stored `ChannelMessage`. Browser code does not call it. An SSE browser only receives events, while a WebSocket browser sends `{ type, data }` through its connection.

## Consume In The Browser

Use the browser-safe `tako.sh/client` entry point outside React. Pass the exact wire name, `"ws"` for a WebSocket channel, and the channel params:

```ts
import { Channel, type ChannelMessage } from "tako.sh/client";

const events = new Channel("site-events", "ws");
const connection = events.connect({ authorization: token });

(connection.raw as WebSocket).addEventListener("message", (event) => {
  const message = JSON.parse(event.data) as ChannelMessage;
  console.log(message.type, message.data);
});
```

For an SSE channel, omit the transport and call `subscribe()`:

```ts
const updates = new Channel("announcements", undefined, { clientVersion: "2.4.0" });
const subscription = updates.subscribe({ authorization: token });
```

React apps can use `useChannel` from `tako.sh/react`. SSE is the default; set `transport: "ws"` to receive a `send` function:

```tsx
import { useChannel } from "tako.sh/react";

function LiveStatus({ token }: { token: string }) {
  const { messages, status, send } = useChannel("site-events", {
    transport: "ws",
    authorization: token,
  });

  return (
    <button
      disabled={status !== "open"}
      onClick={() => send({ type: "presence", data: { online: 42 } })}
    >
      Send presence event ({messages.length} received)
    </button>
  );
}
```

## Recover After Disconnects

Each publish is inserted before delivery. `replayWindowMs` defaults to 10 minutes, so a reconnect can bridge a browser reload, laptop sleep, network change, server restart, or rolling deploy.

Replay cleanup runs every second, including when no clients are connected. Channel definitions carry their lifecycle settings with server-side publishes, so retention applies before the first subscription. Connections share an app-wide change poller; database reads run outside the async request workers. Shared Postgres stores detect publishes from every server.

- SSE resumes with `Last-Event-ID`; `subscribe({ lastEventId })` sets an initial cursor. The built-in fetch-based SSE reader reconnects until `close()` is called and carries its latest message id forward.
- WebSocket resumes with `last_message_id`; `connect({ lastMessageId })` sets it. `useChannel` tracks the latest id and reconnects WebSockets with bounded backoff and jitter. A lower-level `connect()` call returns one socket, so non-React code owns WebSocket reconnection.
- With no cursor, Tako starts at the latest retained message and then follows live traffic.

If an SSE cursor is older than retained replay, Tako responds with `410 Gone`; WebSocket closes with code `4410` and reason `replay-too-old`. Reload current state from the app's normal data API, then create a fresh subscription rather than assuming the missing events still exist.

## Deploy Replay Storage

Storage is a deployment choice; application publish and subscribe code stays the same.

| Deployment                  | Replay storage                                                   |
| --------------------------- | ---------------------------------------------------------------- |
| Local development           | In-memory for the current dev daemon process                     |
| One production server       | Tako-managed SQLite (`channels.sqlite`) by default               |
| Multiple production servers | Shared Postgres schema `tako_channels`, keyed by deployed app id |

A multi-server environment with channels requires the shared `postgres_url` environment credential so publishes and replay are visible on every server:

```bash
tako credentials set postgres_url --env production
```

Tako validates this before build and deploy work starts. `postgres_url` is an encrypted provider credential, not a `tako.toml` field or an app secret. A single-server environment can also use Postgres by setting the credential.

## Keep Canonical History In Your App Database

The replay store is a short delivery buffer, not permanent product history. Chat messages, document operations, audit events, and anything else that must remain queryable should be committed to your app database before publishing the corresponding channel event.

```ts
const saved = await db.notifications.create({ userId, text });

await siteEvents.publish({
  type: "notification",
  data: saved,
});
```

For canonical writes, use a normal authenticated app endpoint to commit the record, then publish the resulting channel event from server-side code. Treat browser-sent WebSocket frames as transient live signals. The app database remains the source of truth; channel replay only closes short delivery gaps.
