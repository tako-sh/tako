---
title: "A Self-Hosted Pusher and Ably Alternative: Tako Channels"
date: "2026-04-27T14:12"
description: "Pusher charges per connection, Ably per minute. Tako Channels ships SSE, WebSockets, and replay into your own server — for whatever your VPS already costs."
image: 4c95f9258606
---

Most apps need real-time eventually. A chat pane, a live dashboard, a presence indicator on a doc. The default answer is to reach for [Pusher](https://pusher.com/channels/) or [Ably](https://ably.com/) — both excellent products that have been doing this since long before "real-time" was a checkbox on every framework's homepage. Sign up, add an SDK, ship.

The catch is the bill. Both services price per connection, and connections add up fast. A modest app with 5,000 concurrent browsers parked on a dashboard is on Pusher's $499/month tier. Ably's per-minute model gets cheaper at low usage but climbs the same curve once a few thousand users are connected for any length of time.

Tako Channels is the same primitive — durable pub/sub with SSE, WebSockets, replay, and per-channel auth — built directly into the proxy that's already serving your app. Your $5 VPS doesn't know or care how many sockets it's holding open.

## At a glance

|                      | **Pusher Channels**       | **Ably**                     | **Tako Channels**                                              |
| -------------------- | ------------------------- | ---------------------------- | -------------------------------------------------------------- |
| **Hosting**          | SaaS                      | SaaS                         | Self-hosted (your VPS)                                         |
| **Free tier**        | 100 conns / 200k msg/day  | 200 conns / 6M msg/mo        | Whatever the box can hold                                      |
| **Next paid tier**   | $49/mo — 500 conns        | $29/mo + usage — 10k conns   | $0 extra                                                       |
| **5k concurrent**    | $299/mo (Business)        | Pro tier $399/mo + usage     | $0 extra                                                       |
| **Transports**       | WebSocket                 | WebSocket, SSE, MQTT         | WebSocket, SSE                                                 |
| **Replay / history** | Add-on (Storage)          | 24h–72h replay (longer paid) | Bounded replay window, default 24h, [tunable](/docs/tako-toml) |
| **Presence**         | Yes (built-in)            | Yes (built-in)               | Not yet — auth callback stamps a user ID per connection        |
| **Per-channel auth** | Auth endpoint in your app | Token request in your app    | [`auth` callback](/blog/durable-channels-built-in) in your app |
| **Server publish**   | REST API                  | REST or realtime SDK         | Direct module import — typed                                   |
| **Pattern matching** | Wildcard subscriptions    | Wildcard subscriptions       | Hono-style patterns (`chat/:roomId`)                           |

Sources: [Pusher pricing](https://pusher.com/channels/pricing/), [Ably pricing](https://ably.com/pricing) (April 2026).

## SDK code, side by side

Pusher's API is the canonical real-time SDK shape — server triggers, client subscribes:

```ts
// Server (Node)
import Pusher from "pusher";
const pusher = new Pusher({ appId, key, secret, cluster: "us2" });
await pusher.trigger("chat-room-42", "msg", { text: "hello" });

// Client (browser)
import Pusher from "pusher-js";
const channel = new Pusher(key, { cluster: "us2" }).subscribe("chat-room-42");
channel.bind("msg", (data) => console.log(data));
```

The Tako shape is similar in spirit but file-based — channel definitions live next to your app code and the proxy discovers them at deploy time:

```ts
// channels/chat.ts
import { defineChannel } from "tako.sh";

export default defineChannel({
  name: "chat",
  paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
  auth: {
    headerName: "authorization",
    verify: async ({ header, params }) => {
      const session = await readSession(header);
      if (!session || !canReadRoom(session.userId, params.roomId)) return false;
      return { subject: session.userId };
    },
  },
}).$messageTypes<{ msg: { text: string } }>();
```

```ts
// Server-side publish — typed, imported directly
import chat from "../channels/chat";
await chat({ roomId: "42" }).publish({ type: "msg", data: { text: "hello" } });
```

```tsx
// Client (React)
import { useChannel } from "tako.sh/react";
const { messages } = useChannel("chat", { params: { roomId: "42" } });
```

There's no app key, no cluster, no auth endpoint to stand up separately. The auth callback runs inside your app on every connection and can hit your session store, your database, your feature flags — whatever "is this user allowed in this room" already means in your code. See the [Durable Channels announcement](/blog/durable-channels-built-in) for the full surface, or the [docs](/docs/how-tako-works) for the protocol.

## How the request actually flows

```d2
direction: right

client: Browser {style.fill: "#9BC4B6"; style.font-size: 16}
proxy: Tako Proxy {style.fill: "#E88783"; style.font-size: 16}
app: Your App {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
store: SQLite replay {style.fill: "#E88783"; style.font-size: 16}

client -> proxy: "GET /_tako/channels/chat?roomId=42"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> store: "replay from Last-Event-ID"
proxy -> client: "SSE / WebSocket stream"
```

The Tako proxy owns `/_tako/channels/<name>` directly. Your app never holds the socket — it only answers an auth question per connection. When `tako-server` upgrades or your app rolls, the proxy keeps the stream open and re-asks for auth on reconnect. That's the part that's hard to do yourself with a hand-rolled WebSocket gateway, and it's the same job Pusher and Ably charge you to do at the edge.

## What Tako doesn't do (yet)

Honest call-out: Pusher and Ably both ship **presence channels** — a server-maintained list of who's currently subscribed, with join/leave events. Tako doesn't have that primitive yet. The auth callback stamps a stable `subject` (typically a user ID) on every connection, so you can build a presence list yourself by publishing join/leave messages from your auth callback into a sibling channel — but it's not a one-line config.

The roadmap covers it, alongside [durable workflows](/blog/durable-workflows-are-here) (already shipped) and queues. The pattern is the same as channels: things most apps bolt on as separate services, served directly by the proxy your app is already running behind. See [Build Your Own Edge Network](/blog/build-your-own-edge-network-on-commodity-hardware) for where this is heading.

## Pricing reality check

For a typical indie or small-team app, the connection math goes like this:

| Concurrent users | Pusher tier        | Ably tier (per-minute)     | Tako on a $5 VPS            |
| ---------------- | ------------------ | -------------------------- | --------------------------- |
| 100              | Sandbox (free)     | Free                       | $5/mo                       |
| 1,000            | Pro — $99/mo       | Standard — ~$30/mo + usage | $5/mo                       |
| 5,000            | Business — $299/mo | Pro — $399/mo + usage      | $5/mo                       |
| 20,000           | Plus — $899/mo     | Pro — $399/mo + usage      | A bigger VPS — maybe $40/mo |

A single modest VPS comfortably holds tens of thousands of idle WebSocket connections — the bottleneck is usually message throughput, not connection count. If you outgrow one box, [add another](/docs/deployment) — same `tako.toml`, same channels, the proxy fans out.

## When each makes sense

Pick **Pusher or Ably** if you need presence today, want to outsource the operational load entirely, or need a feature like MQTT bridging that lives on the SaaS side. Both are great products run by good teams.

Pick **Tako Channels** if you'd rather not pay per connection, you already run a VPS (or want to), and you want real-time as a primitive of the same server that's [serving your HTTP traffic](/blog/pingora-vs-caddy-vs-traefik), [holding your secrets](/blog/secrets-without-env-files), and [running your workflows](/blog/durable-workflows-are-here). One binary, one bill, and the connections are free.

`tako init`, drop a file in `channels/`, `tako dev`, and you have a real-time feature running locally over [real HTTPS](/blog/local-dev-with-real-https) in about a minute. [Start with the docs →](/docs)
