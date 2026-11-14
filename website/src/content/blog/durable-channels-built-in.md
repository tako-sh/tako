---
title: "Durable Channels, Built In"
date: "2026-04-13T01:46"
description: "Tako ships durable WebSocket and SSE channels with bounded replay, reconnection, and per-channel auth — no Pusher, no Redis, no sidecars."
image: c61fe7054a9c
---

Most apps need real-time eventually. A chat pane, a live dashboard, a collaborative cursor, a webhook fanning out to connected clients. The path there is familiar: stand up a Pusher account, glue together Redis pub/sub and a WebSocket gateway, or pay Ably per connection. One more service, one more bill, one more thing to keep alive.

Tako ships this as a built-in primitive. One channel definition gives you an authenticated pub/sub route on your own server, with SSE and WebSocket transports, reconnect replay, and per-channel auth served directly by the Tako proxy.

## How it works

A channel is a named route your app defines. The Tako proxy owns the public endpoint at `/_tako/channels/ws/<name>` (dedicated WebSocket) or `/_tako/channels/sse/<name>` (SSE), handles the SSE or WebSocket handshake, stores published messages in a bounded replay log, and asks your app for an auth decision on every connection.

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
  transport: "ws",
}).$messageTypes<ChatMessages>();
```

Params do not partition replay or fanout. All authorized `roomId` bindings to `chat` read the same stream, so this example is not a private-room implementation.

For dedicated WebSockets, typed params travel as query parameters, so clients connect to paths like `/_tako/channels/ws/chat?roomId=lobby`. The verify callback runs inside your app, so it can touch your session store, database, feature flags, or whatever "is this user allowed" already means in your code.

```d2
direction: right

client: Client {style.fill: "#9BC4B6"; style.font-size: 16}
proxy: Tako Proxy {style.fill: "#E88783"; style.font-size: 16}
app: Your App {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
store: SQLite replay {style.fill: "#E88783"; style.font-size: 16}

client -> proxy: "GET /_tako/channels/ws/chat?roomId=lobby"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> store: "replay from cursor"
proxy -> client: "WebSocket stream"
```

## The durable part

"Durable" means published channel messages are stored before delivery and retained for a bounded replay window. The default window is 10 minutes, which is enough to bridge the cases that make realtime apps feel flaky: laptop sleep, mobile network handoff, browser reloads, clean connection rotation, and short server restarts.

When a client reconnects with `Last-Event-ID` for SSE or `last_message_id` for WebSocket, the proxy replays everything it still has, in order, then hands off to the live tail. If a cursor is older than the replay window, SSE returns `410 Gone` and WebSocket closes with code `4410` (`replay-too-old`) so the client can fall back to the app's normal data-loading flow instead of silently skipping events.

Each channel has four lifecycle knobs:

| Setting                   | Default    | What it controls                         |
| ------------------------- | ---------- | ---------------------------------------- |
| `replayWindowMs`          | 10 minutes | How far back reconnecting clients can go |
| `inactivityTtlMs`         | off        | Drop channel state after idle period     |
| `keepaliveIntervalMs`     | 25s        | SSE/WS heartbeat cadence                 |
| `maxConnectionLifetimeMs` | 2 hours    | Cap on a single connection's lifetime    |

The replay log is not a replacement for your product database. Chat messages, document operations, and audit history still belong in app-owned storage when they are canonical. Channels handle delivery and short reconnect replay so the backend can publish once without checking whether a browser happens to be connected.

## Publish from another app

An app named `web` owns the channel above as `web.chat`. Another managed app in the same network and environment can publish to it through a string address:

```ts
import { channel } from "tako.sh";

await channel("web.chat", { roomId: "lobby" }).publish({
  type: "msg",
  data: { text: "Your report is ready", userId: "system" },
});
```

Run `tako gen` in `web`, add `schemas = ["../web/tako-schema.json"]` at the root of the caller's `tako.toml`, and run `tako gen` there too. Generated types check the address, params, message name, and payload without importing the web app's source. Go and Rust callers can compile the same schema into their own DTOs.

The publication uses the recipient's channel definition and acknowledges publication; it does not wait for browsers to process the message. Subscriber authorization still applies to connections, and `roomId` still does not partition the stream. Any app in the same network and environment can publish, while public internet clients and generic private peers cannot invoke the internal primitive. See [Network Primitives](/docs/network-primitives/) for transport, language support, and delivery limits.

## Why we built it in

Most deploy tools stop at "get your code running." Kamal, Dokku, Coolify — they ship your container, point a proxy at it, and hand you the keys. Anything your app needs beyond HTTP is something you glue in yourself.

We think that's a weird place to stop. A proxy that already terminates TLS, tracks connected clients, and survives your app restarting is in the right position to own durable channels too. It's less code in your app, one less service to run, and one less vendor on your invoice.

Durable channels sit alongside [workflows](/docs/workflows/), [network handlers](/docs/network-primitives/), and [image optimization](/docs/images/) in Tako's platform layer. Try them with `tako init`, add a channel, and run `tako dev` for a realtime feature over [local HTTPS](/blog/local-dev-with-real-https/). See the [Channels reference](/docs/channels/) for the current contract.
