---
title: "Durable Channels, Built In"
date: "2026-04-13T01:46"
description: "Tako ships durable WebSocket and SSE channels with bounded replay, reconnection, and per-channel auth — no Pusher, no Redis, no sidecars."
image: c61fe7054a9c
---

> **Updated November 15, 2026:** This article now uses params-scoped streams and the shared WS default. The old topic-only URL shape has been replaced by separate WS/SSE paths; dedicated WS remains available. See [Channels](/docs/channels/) for the current API and upgrade behavior.

Most apps need real-time eventually. A chat pane, a live dashboard, a collaborative cursor, a webhook fanning out to connected clients. The path there is familiar: stand up a Pusher account, glue together Redis pub/sub and a WebSocket gateway, or pay Ably per connection. One more service, one more bill, one more thing to keep alive.

Tako ships this as a built-in primitive. One channel definition gives you an authenticated pub/sub route on your own server, with SSE and WebSocket transports, reconnect replay, and per-channel auth served directly by the Tako proxy.

## How it works

A channel is a named stream your app defines. The browser SDK shares `/_tako/channels/ws` across WS channels and Presence, and the proxy asks your app for an auth decision on every logical subscription. SSE uses a separate request at `/_tako/channels/sse/<name>`; opting into dedicated WS uses `/_tako/channels/ws/<name>`. Durable channels store published messages in a bounded replay log before delivery.

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
      if (!session || !(await canJoinRoom(session.userId, input.params.roomId))) return false;
      return { subject: session.userId };
    },
  },
  transport: "ws",
}).$messageTypes<ChatMessages>();
```

`authenticate` and `canJoinRoom` are app-owned helpers. The channel name and validated `roomId` select separate replay and fanout streams. The verifier must check room access; knowing a room ID is not authorization.

Shared WS sends the topic, typed params, token, and replay cursor in a subscription command. For dedicated WebSockets, params travel in URLs such as `/_tako/channels/ws/chat?roomId=lobby`. The verify callback runs inside your app, so it can use your session store, database, and permission checks.

```d2
direction: right

client: Client {style.fill: "#9BC4B6"; style.font-size: 16}
proxy: Tako Proxy {style.fill: "#E88783"; style.font-size: 16}
app: Your App {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
store: SQLite replay {style.fill: "#E88783"; style.font-size: 16}

client -> proxy: "GET /_tako/channels/ws\nsubscribe: chat, roomId=lobby"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> store: "replay from cursor"
proxy -> client: "logical subscription data"
```

## The durable part

"Durable" means published channel messages are stored before delivery and retained for a bounded replay window. The default window is 10 minutes, which is enough to bridge the cases that make realtime apps feel flaky: laptop sleep, mobile network handoff, browser reloads, clean connection rotation, and short server restarts.

When a client reconnects with `Last-Event-ID` for SSE or `connect({ lastMessageId })` for WS, the proxy replays retained messages after that cursor, then follows live traffic. Shared WS carries a separate `cursor` per subscription; dedicated WS uses `last_message_id`. If history has expired, SSE returns `410 Gone` and WS closes that subscription with code `4410` (`replay-too-old`). Use the SDK's recovery callback to reload application state.

Each channel has four lifecycle knobs:

| Setting                   | Default    | What it controls                         |
| ------------------------- | ---------- | ---------------------------------------- |
| `replayWindowMs`          | 10 minutes | How far back reconnecting clients can go |
| `inactivityTtlMs`         | off        | Drop channel state after idle period     |
| `keepaliveIntervalMs`     | 25s        | SSE/dedicated WS heartbeat cadence; shared WS has a fixed physical heartbeat |
| `maxConnectionLifetimeMs` | 2 hours    | Cap on a subscription's authorized lifetime |

The replay log is not a replacement for your product database. Chat messages, document operations, and audit history still belong in app-owned storage when they are canonical. Channels handle delivery and short reconnect replay so the backend can publish once without checking whether a browser happens to be connected.

Use `ephemeral: true` for events that should only reach currently connected readers. Shared sockets permit 32 subscriptions, 64 KiB client commands, and 1 MiB delivered events. Closing a subscription removes only its topic or presence membership; the last subscription closes the socket. After cookie-session changes, call `resetChannelConnections()` so new subscriptions use fresh handshake cookies. The [shared ownership guide](/docs/channels/#shared-websocket-ownership) covers cleanup, limits, and session changes.

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

The publication uses the recipient's channel definition and acknowledges publication; it does not wait for browsers to process the message. Only subscribers authorized for the same channel and params receive it. Any managed app in the same network and environment can publish, while public internet clients and generic private peers cannot invoke the internal primitive. See [Network Primitives](/docs/network-primitives/) for transport, language support, and delivery limits.

## Why we built it in

Most deploy tools stop at "get your code running." Kamal, Dokku, Coolify — they ship your container, point a proxy at it, and hand you the keys. Anything your app needs beyond HTTP is something you glue in yourself.

We think that's a weird place to stop. A proxy that already terminates TLS, tracks connected clients, and survives your app restarting is in the right position to own durable channels too. It's less code in your app, one less service to run, and one less vendor on your invoice.

Durable channels sit alongside [workflows](/docs/workflows/), [network handlers](/docs/network-primitives/), and [image optimization](/docs/images/) in Tako's platform layer. Try them with `tako init`, add a channel, and run `tako dev` for a realtime feature over [local HTTPS](/blog/local-dev-with-real-https/). See the [Channels reference](/docs/channels/) for the current contract.
