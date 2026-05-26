---
title: "Durable Channels, Built In"
date: "2026-04-13T01:46"
description: "Tako ships durable WebSocket and SSE channels with bounded replay, reconnection, and per-channel auth — no Pusher, no Redis, no sidecars."
image: c61fe7054a9c
---

Most apps need real-time eventually. A chat pane, a live dashboard, a collaborative cursor, a webhook fanning out to connected clients. The path there is familiar: stand up a Pusher account, glue together Redis pub/sub and a WebSocket gateway, or pay Ably per connection. One more service, one more bill, one more thing to keep alive.

Tako ships this as a built-in primitive. One channel definition gives you an authenticated pub/sub route on your own server, with SSE and WebSocket transports, reconnect replay, and per-channel auth served directly by the Tako proxy.

## How it works

A channel is a named route your app defines. The Tako proxy owns the public endpoint at `/_tako/channels/<name>`, handles the SSE or WebSocket handshake, stores published messages in a bounded SQLite replay log, and asks your app for an auth decision on every connection.

```ts
// src/channels/chat.ts
import { defineChannel } from "tako.sh";

type ChatMessages = {
  msg: { text: string; userId: string };
  typing: { userId: string };
};

export default defineChannel("chat", {
  paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
  auth: {
    headerName: "authorization",
    async verify(input) {
      const session = await authenticate(input.header);
      if (!session) return false;
      return { subject: session.userId };
    },
  },
  handler: {
    msg: async (data) => data,
    typing: async (data) => data,
  },
}).$messageTypes<ChatMessages>();
```

Typed params travel as query parameters, so clients connect to paths like `/_tako/channels/chat?roomId=lobby`. The verify callback runs inside your app, so it can touch your session store, database, feature flags, or whatever "is this user allowed" already means in your code.

```d2
direction: right

client: Client {style.fill: "#9BC4B6"; style.font-size: 16}
proxy: Tako Proxy {style.fill: "#E88783"; style.font-size: 16}
app: Your App {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
store: SQLite replay {style.fill: "#E88783"; style.font-size: 16}

client -> proxy: "GET /_tako/channels/chat?roomId=lobby"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> store: "replay from cursor"
proxy -> client: "SSE / WebSocket stream"
```

## The durable part

"Durable" means published channel messages are stored before delivery and retained for a bounded replay window. The default window is 10 minutes, which is enough to bridge the cases that make realtime apps feel flaky: laptop sleep, mobile network handoff, browser reloads, clean connection rotation, and short server restarts.

When a client reconnects with `Last-Event-ID` for SSE or `last_message_id` for WebSocket, the proxy replays everything it still has, in order, then hands off to the live tail. If a cursor is older than the replay window, the proxy returns `410 Gone` so the client can fall back to the app's normal data-loading flow instead of silently skipping events.

Each channel has four lifecycle knobs:

| Setting                   | Default    | What it controls                         |
| ------------------------- | ---------- | ---------------------------------------- |
| `replayWindowMs`          | 10 minutes | How far back reconnecting clients can go |
| `inactivityTtlMs`         | off        | Drop channel state after idle period     |
| `keepaliveIntervalMs`     | 25s        | SSE/WS heartbeat cadence                 |
| `maxConnectionLifetimeMs` | 2 hours    | Cap on a single connection's lifetime    |

The replay log is not a replacement for your product database. Chat messages, document operations, and audit history still belong in app-owned storage when they are canonical. Channels handle delivery and short reconnect replay so the backend can publish once without checking whether a browser happens to be connected.

## Why we built it in

Most deploy tools stop at "get your code running." Kamal, Dokku, Coolify — they ship your container, point a proxy at it, and hand you the keys. Anything your app needs beyond HTTP is something you glue in yourself.

We think that's a weird place to stop. A proxy that already terminates TLS, tracks connected clients, and survives your app restarting is in the right position to own durable channels too. It's less code in your app, one less service to run, and one less vendor on your invoice.

Durable channels are one piece of Tako's platform layer. The [roadmap](/blog/build-your-own-edge-network-on-commodity-hardware/) also includes queues, scheduled workflows, and image optimization — the primitives apps actually need, right where your app already lives. Try them today: `tako init`, add a channel, `tako dev`, and you have a realtime feature running locally over [real HTTPS](/blog/local-dev-with-real-https/). See the [docs](/docs/how-tako-works/) for the protocol details.
