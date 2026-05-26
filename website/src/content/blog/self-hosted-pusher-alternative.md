---
title: "A Self-Hosted Pusher and Ably Alternative: Tako Channels"
date: "2026-04-27T14:12"
description: "Pusher charges per connection, Ably per minute. Tako Channels puts durable WebSocket and SSE communication into your own server."
image: 4c95f9258606
---

Most apps need realtime eventually. A chat pane, a live dashboard, a presence indicator on a doc. The default answer is to reach for [Pusher](https://pusher.com/channels/) or [Ably](https://ably.com/) — both excellent products that have been doing this since long before "realtime" was a checkbox on every framework's homepage. Sign up, add an SDK, ship.

The catch is the bill. Both services price per connection, and connections add up fast. A modest app with 5,000 concurrent browsers parked on a dashboard is on Pusher's $499/month tier. Ably's per-minute model gets cheaper at low usage but climbs the same curve once a few thousand users are connected for any length of time.

Tako Channels are the self-hosted version of that shape: WebSocket and SSE routes served by the same proxy already running your app. Every publish is stored in a bounded replay log before delivery, so reconnecting clients can catch up without the backend checking whether a browser is connected. Your $5 VPS does not know or care how many sockets it is holding open.

## At a glance

|                      | **Pusher Channels**       | **Ably**                     | **Tako Channels**                             |
| -------------------- | ------------------------- | ---------------------------- | --------------------------------------------- |
| **Hosting**          | SaaS                      | SaaS                         | Self-hosted (your VPS)                        |
| **Free tier**        | 100 conns / 200k msg/day  | 200 conns / 6M msg/mo        | Whatever the box can hold                     |
| **Next paid tier**   | $49/mo — 500 conns        | $29/mo + usage — 10k conns   | $0 extra                                      |
| **5k concurrent**    | $299/mo (Business)        | Pro tier $399/mo + usage     | $0 extra                                      |
| **Live transport**   | WebSocket                 | WebSocket, SSE, MQTT         | WebSocket, SSE                                |
| **Replay / history** | Add-on (Storage)          | 24h-72h replay (longer paid) | Bounded channel replay, 10 minutes by default |
| **Presence**         | Yes (built-in)            | Yes (built-in)               | Not yet — channel auth stamps a subject       |
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
      if (!session || !canReadRoom(session.userId, params.roomId)) return false;
      return { subject: session.userId };
    },
  },
  handler: {
    typing: (data) => data,
  },
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
const { messages } = useChannel("presence", { params: { roomId: "42" } });
```

There is no app key, no cluster, no auth endpoint to stand up separately. The auth callback runs inside your app on every connection and can hit your session store, database, feature flags — whatever "is this user allowed in this room" already means in your code.

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

Channels use the same public route for live delivery and resume:

```txt
GET /_tako/channels/chat?roomId=42
```

That window is for delivery, not product history. A collaborative cursor can be replayed after a laptop wakes. A chat message should still be written to your app database by your channel handler or route before it becomes canonical.

## How the request actually flows

```d2
direction: right

client: Browser {style.fill: "#9BC4B6"; style.font-size: 16}
proxy: Tako Proxy {style.fill: "#E88783"; style.font-size: 16}
app: Your App {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}

client -> proxy: "GET /_tako/channels/presence?roomId=42"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> client: "SSE / WebSocket live frames"
```

The Tako proxy owns `/_tako/channels/<name>` directly. Your app does not have to host a separate socket gateway; it answers auth and publishes typed events. When `tako-server` upgrades or your app rolls, clients reconnect through the same public route and the SDK rebuilds the live subscription.

## What Tako does not do yet

Honest call-out: Pusher and Ably both ship **presence channels** — a server-maintained list of who is currently subscribed, with join/leave events. Tako does not have that primitive yet. The auth callback stamps a stable `subject` (typically a user ID) on every connection, which gives us the right foundation, but presence should become a first-class channel feature rather than a userland workaround.

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

Pick **Pusher or Ably** if you need managed global fanout, presence today, MQTT bridging, enterprise SLAs, or you simply want someone else to operate realtime for you.

Pick **Tako Channels** if you already run a VPS, do not want to pay per connection, and want durable realtime as a primitive of the same server that is [serving your HTTP traffic](/blog/pingora-vs-caddy-vs-traefik/), [holding your secrets](/blog/secrets-without-env-files/), and [running your workflows](/blog/durable-workflows-are-here/).

`tako init`, drop a file in `src/channels/`, `tako dev`, and you have a realtime feature running locally over [real HTTPS](/blog/local-dev-with-real-https/) in about a minute. [Start with the docs ->](/docs/)
