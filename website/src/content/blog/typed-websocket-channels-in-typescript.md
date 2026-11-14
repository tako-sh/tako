---
title: "Typed WebSocket Channels in TypeScript: Params, Auth, and Live Pub/Sub"
seoTitle: "Typed WebSocket Channels in TypeScript"
date: "2026-05-03T05:54"
description: "Tako channels turn TypeBox params, auth, typed publish payloads, and browser WebSocket/SSE wiring into one TypeScript realtime model."
image: a5263b3d6adb
---

WebSocket code usually starts clean and then quietly splits into several little protocols: one for route params, one for auth, one for publish payloads, one for reconnects, and one for whatever the browser cannot express in `new WebSocket()`.

Tako channels try to make the live part feel like one model. You define a channel once, and Tako turns that definition into the public route, the param validator, the auth callback, the publish type, and the browser transport.

Durability is part of the channel contract, but it is scoped to delivery. Every publish is written to a bounded replay log before fanout, so reconnecting clients can catch up after short disconnects without turning the channel into your app's permanent history API.

## Parameter safety

The `roomId` examples below demonstrate typed connection context, not isolated rooms. In the current protocol, replay and fanout are keyed by the channel name: all authorized bindings to `presence` or `chat` receive that channel's messages, regardless of params. A per-room authorization check does not partition delivery. Do not use these examples for private room traffic; see the [Channels reference](/docs/channels/#understand-channel-params).

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
      return canAccessRoom(session.userId, params.roomId, operation)
        ? { subject: session.userId }
        : false;
    },
  },
  transport: "ws",
}).$messageTypes<PresenceMessages>();
```

TypeBox matters here because it gives Tako both sides of the shape. Per the [TypeBox docs](https://github.com/sinclairzx81/typebox), a schema is a JSON Schema object that can also infer a TypeScript type. Tako uses that JSON Schema to validate `/_tako/channels/ws/presence?roomId=lobby` before asking your app to authorize anything, while TypeScript uses the same declaration to type `params.roomId` in your callback.

The default transport is receive-only SSE. With `transport: "ws"`, the proxy stores and broadcasts client `{ type, data }` frames as sent. Handle application mutations through your app's HTTP endpoints. See the [Channels reference](/docs/channels/) for the current contract.

## Auth happens before messages

For normal SSE requests, auth can ride in headers or cookies. Browser WebSockets are trickier because the constructor does not let client code set arbitrary headers. Tako handles that with a first text frame:

```json
{ "type": "tako.auth", "token": "Bearer abc" }
```

If a channel requires header auth and the WebSocket upgrade did not include that header, the proxy waits briefly for this frame. It parses the token as the declared header value, asks your app's `verify` callback through `POST /channels/authorize`, and only then starts accepting publish frames.

That keeps auth in one place: your callback receives `{ header, cookie, params, channel, operation }`, and `operation` tells you whether the request is subscribing or connecting. Cookie-only auth still works too; set `headerName: false` and `cookieName` in the channel definition.

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

browser -> proxy: "GET /_tako/channels/ws/presence?roomId=lobby\nUpgrade: websocket"
browser -> proxy: "first frame: tako.auth"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject"
proxy -> browser: "live ChannelMessage frames"
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

SSE subscriptions reconnect automatically, and React `useChannel` reconnects WebSockets with the last received message id. A lower-level `Channel.connect()` returns one socket; its caller owns reconnection. Replay only covers messages still inside the retention window.

That window is intentionally short by default: 10 minutes.

## Store canonical history in your app

Channel replay is for delivery continuity, not product history. If the event is part of your app's source of truth, write it through a normal authenticated app endpoint, then publish the saved value from server-side code:

```ts
import chat from "../channels/chat";

const message = await db.messages.insert({ roomId, text, userId });
await chat({ roomId }).publish({ type: "msg", data: message });
```

That keeps the TypeScript surface small: one channel primitive for WebSocket/SSE delivery, with app storage used when messages need to outlive the reconnect window.
