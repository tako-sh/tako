---
title: "Durable Channels, Built In"
date: "2026-04-13T01:46"
description: "Tako now ships durable WebSocket and SSE channels with replay, reconnection, and per-channel auth — no Pusher, no Redis, no sidecars."
image: c61fe7054a9c
---

Most apps need real-time eventually. A chat pane, a live dashboard, a collaborative cursor, a webhook fanning out to connected clients. The path there is depressingly familiar: stand up a Pusher account, or glue together Redis pub/sub and a WebSocket gateway, or pay Ably per connection. One more service, one more bill, one more thing to keep alive.

Tako now ships this as a built-in primitive. Two new lines in your app give you a durable, authenticated pub/sub channel on your own server, with SSE and WebSocket transports, replay across reconnects, and per-channel auth — served directly by the Tako proxy.

## How it works

A channel is just a named stream. Your app defines it and declares who can read or write. The Tako proxy owns the public endpoint at `/channels/<name>`, handles the SSE or WebSocket handshake, persists messages to a small SQLite store on the server, and asks your app for an auth decision on every connection.

```go
tako.Channels.Register("chat", tako.ChannelDefinition{
  Transport: tako.ChannelTransportWS,
  ParamsSchema: []byte(`{
    "type": "object",
    "properties": { "roomId": { "type": "string" } },
    "required": ["roomId"]
  }`),
  Auth: &tako.ChannelAuthScheme{HeaderName: "authorization"},
  Verify: func(input tako.VerifyInput) tako.ChannelAuthDecision {
    userID := authenticate(input.Header)
    if userID == "" {
      return tako.RejectChannel()
    }
    return tako.AllowChannel(tako.ChannelGrant{Subject: userID})
  },
})
```

The filename or registered name is the channel name; typed params travel as query parameters, so clients connect to paths like `/channels/chat?roomId=lobby`. The verify callback runs inside your app, so it can touch your session store, your database, your feature flags — whatever "is this user allowed" already means in your code. The same callback works in the [JavaScript SDK](/docs).

```d2
direction: right

client: Client {style.fill: "#9BC4B6"; style.font-size: 16}
proxy: Tako Proxy {style.fill: "#E88783"; style.font-size: 16}
app: Your App {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
store: SQLite replay {style.fill: "#E88783"; style.font-size: 16}

client -> proxy: "GET /channels/chat?roomId=lobby"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> store: "replay from Last-Event-ID"
proxy -> client: "SSE / WebSocket stream"
```

## The durable part

"Durable" is the word we chose carefully. Messages published to a channel land in a bounded replay window — 24 hours by default, tunable per channel. When a client reconnects with a `Last-Event-ID` header (SSE) or `last_message_id` query param (WebSocket), the proxy replays everything they missed, in order, and then seamlessly hands off to the live tail.

That means restarts don't drop messages. A bad wifi handoff on a phone doesn't drop messages. A [rolling deploy](/blog/what-happens-when-you-run-tako-deploy) of your app doesn't drop messages — the proxy keeps the stream, your app just gets asked to re-authorize the reconnection. And if a cursor is older than the replay window, the proxy returns `410 Gone` so the client knows to start fresh rather than silently skip events.

Each channel has four knobs on its lifecycle:

| Setting                   | Default  | What it controls                         |
| ------------------------- | -------- | ---------------------------------------- |
| `replayWindowMs`          | 24 hours | How far back reconnecting clients can go |
| `inactivityTtlMs`         | off      | Drop channel state after idle period     |
| `keepaliveIntervalMs`     | 25s      | SSE/WS heartbeat cadence                 |
| `maxConnectionLifetimeMs` | 2 hours  | Cap on a single connection's lifetime    |

Your auth callback can return per-user overrides, so a free-tier subscriber might get a 5-minute replay window while a paid customer gets the full 24 hours — same code path, same channel.

## Why we built it in, not bolted on

The honest answer is: most deploy tools stop at "get your code running." Kamal, Sidekick, Dokku, Coolify — they ship your container, point a proxy at it, and hand you the keys. Anything your app needs beyond HTTP — pub/sub, queues, scheduled work — is something you glue in yourself.

We think that's a weird place to stop. A proxy that already terminates TLS, tracks connected clients, and survives your app restarting is in the exact right position to own the durable channel too. It's less code in your app, one less service to run, and one less vendor on your invoice.

Channels are the first piece. The [roadmap](/blog/build-your-own-edge-network-on-commodity-hardware) also includes durable queues, scheduled workflows, and image optimization — the primitives apps actually need, right where your app already lives. Try them today: `tako init`, add a channel, `tako dev`, and you have a real-time feature running locally over [real HTTPS](/blog/local-dev-with-real-https) in about a minute. See the [docs](/docs/how-tako-works) for the full protocol.
