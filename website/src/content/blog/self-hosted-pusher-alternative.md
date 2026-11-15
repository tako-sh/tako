---
title: "A Self-Hosted Pusher and Ably Alternative: Tako Channels"
date: "2026-04-27T14:12"
description: "Pusher charges per connection, Ably per minute. Tako Channels puts durable WebSocket and SSE communication into your own server."
image: 4c95f9258606
---

> **Updated November 15, 2026:** Tako now includes [Presence](/docs/presence/), params-scoped channel streams, and shared WS subscriptions. The Tako examples below have been updated. Competitor pricing is a historical April 2026 snapshot; check the linked pricing pages before choosing a plan.

Most apps need realtime eventually. A chat pane, a live dashboard, a presence indicator on a doc. The default answer is to reach for [Pusher](https://pusher.com/channels/) or [Ably](https://ably.com/) — both excellent products that have been doing this since long before "realtime" was a checkbox on every framework's homepage. Sign up, add an SDK, ship.

The catch is the bill. Connection and usage charges add up as more browsers stay on a dashboard. The tables below record the plans compared when this article was first published, rather than current price quotes.

Tako Channels are the self-hosted version of that shape: WebSocket and SSE routes served by the same proxy already running your app. Durable publishes are stored in a bounded replay log before delivery, so reconnecting clients can catch up without the backend checking whether a browser is connected. You pay for server capacity; socket count, fanout, and message sizes still consume its resources.

## At a glance

|                      | **Pusher Channels**       | **Ably**                     | **Tako Channels**                             |
| -------------------- | ------------------------- | ---------------------------- | --------------------------------------------- |
| **Hosting**          | SaaS                      | SaaS                         | Self-hosted (your VPS)                        |
| **Free tier**        | 100 conns / 200k msg/day  | 200 conns / 6M msg/mo        | Whatever the box can hold                     |
| **Next paid tier**   | $49/mo — 500 conns        | $29/mo + usage — 10k conns   | $0 extra                                      |
| **5k concurrent**    | $299/mo (Business)        | Pro tier $399/mo + usage     | $0 extra                                      |
| **Live transport**   | WebSocket                 | WebSocket, SSE, MQTT         | WebSocket, SSE                                |
| **Replay / history** | Add-on (Storage)          | 24h-72h replay (longer paid) | Bounded channel replay, 10 minutes by default |
| **Presence**         | Yes (built-in)            | Yes (built-in)               | Yes — scoped membership and expiring metadata |
| **Per-channel auth** | Auth endpoint in your app | Token request in your app    | `auth` callback in your app                   |
| **Server publish**   | REST API                  | REST or realtime SDK         | Direct module import through the Tako SDK     |
| **Routing shape**    | Named channels            | Named channels               | Named channels with typed params              |

Sources: [Pusher pricing](https://pusher.com/channels/pricing/), [Ably pricing](https://ably.com/pricing) (April 2026).

## SDK code, side by side

Pusher's API is the canonical realtime SDK shape — server triggers, client subscribes:

```ts
// Server (Node)
import Pusher from "pusher";
const pusher = new Pusher({ appId, key, secret, cluster: "us2" });
await pusher.trigger("chat-room-42", "typing", { userId: "u_123" });

// Client (browser)
import Pusher from "pusher-js";
const channel = new Pusher(key, { cluster: "us2" }).subscribe("chat-room-42");
channel.bind("typing", (data) => console.log(data));
```

Tako's channel name and validated `roomId` select a separate stream with its own subscribers and replay. The authorization callback must still verify access to that room; params alone grant no permission. See [channel parameter semantics](/docs/channels/#understand-channel-params).

The Tako shape is file-based. Channel definitions live next to your app code and the proxy discovers them at deploy time:

```ts
// src/channels/presence.ts
import { defineChannel } from "tako.sh";

export default defineChannel("presence", {
  paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
  auth: {
    headerName: "authorization",
    verify: async ({ header, params }) => {
      const session = await readSession(header);
      if (!session || !(await canReadRoom(session.userId, params.roomId))) return false;
      return { subject: session.userId };
    },
  },
  transport: "ws",
}).$messageTypes<{ typing: { userId: string } }>();
```

```ts
// Server-side publish — typed, imported directly
import presence from "../channels/presence";
await presence({ roomId: "42" }).publish({
  type: "typing",
  data: { userId: "u_123" },
});
```

```tsx
// Client (React)
import { useChannel } from "tako.sh/react";

export function TypingEvents({ token }: { token: string }) {
  const { messages, status } = useChannel("presence", {
    transport: "ws",
    params: { roomId: "42" },
    authorization: token,
  });
  return <p>{status}: {messages.length} typing events received</p>;
}
```

`readSession` and `canReadRoom` are app-owned helpers; `token` is the current session's bearer credential. There is no separate auth service to stand up. The callback runs inside your app for every logical subscription and can query your existing permission model. The channel named `presence` above carries typing events; for current online membership, use `definePresence` and `usePresence` instead.

## Where replay belongs

Pusher and Ably bundle live delivery and replay into one product surface. Tako does the same at the channel level, but keeps the replay window intentionally short by default:

```ts
// src/channels/chat.ts
import { defineChannel } from "tako.sh";

export default defineChannel("chat", {
  paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
  replayWindowMs: 10 * 60 * 1000,
}).$messageTypes<{ msg: { text: string; userId: string } }>();
```

This definition uses the default SSE transport. Its dedicated HTTP stream supports both live delivery and resume:

```txt
GET /_tako/channels/sse/chat?roomId=42
```

That window is for delivery, not product history. A collaborative cursor can be replayed after a laptop wakes. Write canonical chat messages to your app database through an app route before publishing them.

## How the request actually flows

```d2
direction: right

client: Browser {style.fill: "#9BC4B6"; style.font-size: 16}
proxy: Tako Proxy {style.fill: "#E88783"; style.font-size: 16}
app: Your App {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}

client -> proxy: "GET /_tako/channels/ws\nsubscribe: presence, roomId=42"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> client: "logical subscription data"
```

The browser SDK shares `/_tako/channels/ws` between WS channels and Presence with the same origin, credentials, and transport configuration in one tab. `connection: "dedicated"` selects `/_tako/channels/ws/<name>` instead. SSE always uses a separate request at `/_tako/channels/sse/<name>`. Your app answers auth and publishes events without hosting another socket gateway.

Leaving a page unsubscribes its topics and presence memberships while layout subscriptions remain connected. The last subscription closes the socket. Call `resetChannelConnections()` after cookie-session changes so subsequent subscriptions get fresh handshake credentials. Shared connections have limits of 32 subscriptions, 64 KiB client commands including envelopes, and 1 MiB delivered events. See [shared ownership and recovery](/docs/channels/#shared-websocket-ownership); low-level `connect()` callers own reconnection, while React hooks reconnect automatically.

[Presence](/docs/presence/) now supplies server-maintained member snapshots, trusted subjects, and expiring typing/cursor metadata. Each hook call owns a distinct membership, even when several memberships share a physical socket.

## What Tako does not do yet

Long-term message history is still app-owned. Tako Channels give you delivery and short reconnect replay; your database remains the source of truth for messages that must survive beyond the channel window.

## Pricing reality check

For a typical indie or small-team app, the connection math goes like this:

| Concurrent users | Pusher tier        | Ably tier (per-minute)     | Tako on a $5 VPS            |
| ---------------- | ------------------ | -------------------------- | --------------------------- |
| 100              | Sandbox (free)     | Free                       | $5/mo                       |
| 1,000            | Pro — $99/mo       | Standard — ~$30/mo + usage | $5/mo                       |
| 5,000            | Business — $299/mo | Pro — $399/mo + usage      | $5/mo                       |
| 20,000           | Plus — $899/mo     | Pro — $399/mo + usage      | A bigger VPS — maybe $40/mo |

A single modest VPS can hold many idle WebSocket connections. The bottleneck is usually message throughput and fanout design, not the connection count itself. If you outgrow one box, [add another](/docs/deployment/) — same `tako.toml`, same app model.

## When each makes sense

Pick **Pusher or Ably** if you need managed global fanout, MQTT bridging, enterprise SLAs, or you simply want someone else to operate realtime for you.

Pick **Tako Channels** if you already run a VPS, do not want to pay per connection, and want durable realtime as a primitive of the same server that is [serving your HTTP traffic](/blog/pingora-vs-caddy-vs-traefik/), [holding your secrets](/blog/secrets-without-env-files/), and [running your workflows](/blog/durable-workflows-are-here/).

`tako init`, drop a file in `src/channels/`, `tako dev`, and you have a realtime feature running locally over [real HTTPS](/blog/local-dev-with-real-https/) in about a minute. [Start with the docs ->](/docs/)
