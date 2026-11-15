---
title: "Typed WebSocket Channels in TypeScript: Params, Auth, and Live Pub/Sub"
seoTitle: "Typed WebSocket Channels in TypeScript"
date: "2026-05-03T05:54"
description: "Tako channels turn TypeBox params, auth, typed publish payloads, and browser WebSocket/SSE wiring into one TypeScript realtime model."
image: a5263b3d6adb
---

> **Updated November 15, 2026:** The examples below use shared WebSocket subscriptions and params-scoped streams. Older clients used a socket per topic and older releases did not partition channel delivery by params. Use matching client/server versions and the [current Channels reference](/docs/channels/) when upgrading.

WebSocket code usually starts clean and then quietly splits into several little protocols: one for route params, one for auth, one for publish payloads, one for reconnects, and one for whatever the browser cannot express in `new WebSocket()`.

Tako channels try to make the live part feel like one model. You define a channel once, and Tako turns that definition into the public route, the param validator, the auth callback, the publish type, and the browser transport.

Durability is the default channel contract, scoped to delivery. Durable publishes are written to a bounded replay log before fanout, so reconnecting clients can catch up after short disconnects without turning the channel into your app's permanent history API. Set `ephemeral: true` for transient events that should never replay.

## Parameter safety

The channel name and validated params select a stream. `chat({ roomId: "one" })` and `chat({ roomId: "two" })` have separate subscribers, messages, and replay cursors. Params do not grant access: the verifier must still check that the caller may enter the requested room. See the [Channels reference](/docs/channels/#understand-channel-params).

## The channel is the contract

A JavaScript or TypeScript channel is a default export from `src/channels/*.ts` by default. The first argument is the wire name, `paramsSchema` is a TypeBox schema, `auth.verify` decides access, and `transport: "ws"` selects bidirectional WebSocket transport.

```ts
// src/channels/presence.ts
import { defineChannel } from "tako.sh";

type PresenceMessages = {
  cursor: { x: number; y: number; userId: string };
  typing: { userId: string };
};

export default defineChannel("presence", {
  paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
  auth: {
    headerName: "authorization",
    async verify({ header, params, operation }) {
      const session = await readSession(header);
      if (!session) return false;
      return (await canAccessRoom(session.userId, params.roomId, operation))
        ? { subject: session.userId }
        : false;
    },
  },
  transport: "ws",
}).$messageTypes<PresenceMessages>();
```

`readSession` and `canAccessRoom` are app-owned helpers. TypeBox gives Tako both sides of the shape: a JSON Schema for validation and an inferred TypeScript type. Tako validates `roomId=lobby` from each shared subscription before asking your app to authorize it. The same validation applies to a dedicated socket at `/_tako/channels/ws/presence?roomId=lobby`.

This channel named `presence` carries application events. For server-maintained online membership and expiring typing metadata, use the separate [Presence API](/docs/presence/).

The default transport is receive-only SSE. With `transport: "ws"`, the proxy stores and broadcasts client `{ type, data }` frames as sent. Handle application mutations through your app's HTTP endpoints. See the [Channels reference](/docs/channels/) for the current contract.

## Auth happens before messages

For SSE requests, auth can ride in headers or cookies. Browser WebSockets cannot set arbitrary headers in their constructor. By default, the SDK opens `/_tako/channels/ws` once per origin, credential set, and transport configuration in a tab, then authorizes each topic with a subscription message:

```json
{ "type": "subscribe", "id": "1", "channel": "presence", "query": "roomId=lobby", "token": "Bearer abc" }
```

The proxy maps the token to the channel's declared auth header, calls your app through `POST /channels/authorize`, and acknowledges the authorized subscription. The SDK manages subscription IDs, envelopes, and fragment assembly. Application code uses `connect({ authorization: token })` and `send({ type, data })`.

With `connection: "dedicated"`, the SDK instead opens `/_tako/channels/ws/<name>` and uses a `tako.auth` first frame when the required header was absent from the handshake. That dedicated protocol remains supported; it is not the shared socket's protocol. SSE always keeps its own request at `/_tako/channels/sse/<name>`.

Your callback receives `{ header, cookie, params, channel, operation }`. Cookie-only auth uses `headerName: false` and `cookieName`. After login, logout, or account changes, call `resetChannelConnections()` from `tako.sh/client` once the session response has updated cookies. This forces a fresh handshake; browsers do not expose HttpOnly cookie changes to the pool. See [session changes and shared ownership](/docs/channels/#shared-websocket-ownership).

```d2
direction: right

browser: Browser {
  style.fill: "#9BC4B6"
}
proxy: Tako Proxy {
  style.fill: "#E88783"
}
app: "Your app auth callback" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

browser -> proxy: "GET /_tako/channels/ws\nUpgrade: websocket"
browser -> proxy: "subscribe: presence\nroomId=lobby + token"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> browser: "subscribed + topic data"
```

## Publish and reconnect

Server-side publish is just an import. Parameterized channels are callable, so binding params and publishing a typed message are the same motion:

```ts
import presence from "../channels/presence";

await presence({ roomId: "lobby" }).publish({
  type: "typing",
  data: { userId: "u_123" },
});
```

The `.$messageTypes<PresenceMessages>()` call is type-only, but it is enough for TypeScript to reject the wrong message type or payload shape. At runtime, the proxy stores each publish in the bounded replay log and fans it out to subscribers.

SSE subscriptions reconnect automatically, and React `useChannel` reconnects each WS subscription with its last complete message's cursor. A lower-level `Channel.connect()` returns a logical subscription by default; its caller owns reconnection. Closing it or unmounting a hook leaves other topics connected. The last subscription closes the physical socket. Replay only covers messages still inside the retention window.

That window is intentionally short by default: 10 minutes.

Shared sockets allow 32 subscriptions, 64 KiB client commands including envelopes, and 1 MiB delivered events. Stream large AI output as smaller application events. Transport fragmentation is automatic and adds no batching delay; see [the limits and recovery behavior](/docs/channels/#shared-websocket-ownership).

## Store canonical history in your app

Channel replay is for delivery continuity, not product history. If the event is part of your app's source of truth, write it through a normal authenticated app endpoint, then publish the saved value from server-side code:

```ts
import chat from "../channels/chat";

const message = await db.messages.insert({ roomId, text, userId });
await chat({ roomId }).publish({ type: "msg", data: message });
```

That keeps the TypeScript surface small: one channel primitive for WebSocket/SSE delivery, with app storage used when messages need to outlive the reconnect window.
