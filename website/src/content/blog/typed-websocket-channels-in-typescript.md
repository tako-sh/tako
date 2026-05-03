---
title: "Typed WebSocket Channels in TypeScript: Params, Auth, and Reconnects"
date: "2026-05-03T05:54"
description: "Tako channels turn TypeBox params, auth, typed publish payloads, replay, and reconnects into one TypeScript WebSocket model."
image: 82326ea20e74
---

WebSocket code usually starts clean and then quietly splits into five little protocols: one for route params, one for auth, one for publish payloads, one for replay, and one for reconnects. The types live in one place. Runtime validation lives somewhere else. The browser gets a different auth path because `new WebSocket()` cannot send custom headers.

Tako channels try to make that feel like one model instead. You define a channel once, and Tako turns that definition into the public route, the param validator, the auth callback, the publish type, and the reconnect cursor. It is the same idea behind the [`tako.sh` SDK](/blog/why-tako-ships-an-sdk): app code declares intent, the platform owns the tedious boundary work.

## The channel is the contract

A JavaScript or TypeScript channel is a default export from `channels/*.ts`. The `name` is the wire route, `paramsSchema` is a TypeBox schema, `auth.verify` decides access, and `handler` makes the channel bidirectional over WebSocket.

```ts
// channels/chat.ts
import { defineChannel } from "tako.sh";

type ChatMessages = {
  msg: { text: string; userId: string };
  typing: { userId: string };
};

export default defineChannel({
  name: "chat",
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
  handler: {
    msg: async (data, ctx) => {
      await db.messages.insert({ roomId: ctx.params.roomId, ...data });
      return data;
    },
    typing: async (data) => data,
  },
}).$messageTypes<ChatMessages>();
```

TypeBox matters here because it gives Tako both sides of the shape. Per the [TypeBox docs](https://github.com/sinclairzx81/typebox), a schema is a JSON Schema object that can also infer a TypeScript type. Tako uses that JSON Schema to validate `/channels/chat?roomId=lobby` before asking your app to authorize anything, while TypeScript uses the same declaration to type `params.roomId` in your callback.

No handler means SSE: broadcast-only, replayable, simple. A handler means WebSocket: clients can send JSON frames, each frame routes through the matching handler, and the handler's return value is what gets fanned out. See [How Tako Works](/docs/how-tako-works) for the protocol view.

## Auth happens before messages

For normal SSE requests, auth can ride in headers or cookies. Browser WebSockets are trickier because the constructor does not let client code set arbitrary headers. Tako handles that with a first text frame:

```json
{ "type": "tako.auth", "token": "Bearer abc", "lastMessageId": "123" }
```

If a channel requires header auth and the WebSocket upgrade did not include that header, the proxy waits up to five seconds for this frame. It parses the token as the declared header value, asks your app's `verify` callback through `POST /channels/authorize`, and only then starts replaying or accepting publish frames.

That keeps auth in one place: your callback still receives `{ header, cookie, params, channel, operation }`, and `operation` tells you whether the request is subscribing, connecting, or publishing. Cookie-only auth still works too; set `headerName: false` and `cookieName` in the channel definition.

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
store: "SQLite replay window" {
  style.fill: "#E88783"
}

browser -> proxy: "GET /channels/chat?roomId=lobby\nUpgrade: websocket"
browser -> proxy: "first frame: tako.auth + lastMessageId"
proxy -> app: "POST /channels/authorize"
app -> proxy: "ok + subject + lifecycle"
proxy -> store: "replay after cursor"
proxy -> browser: "ChannelMessage frames"
```

## Publish, replay, reconnect

Server-side publish is just an import. Parameterized channels are callable, so binding params and publishing a typed message are the same motion:

```ts
import chat from "../channels/chat";

await chat({ roomId: "lobby" }).publish({
  type: "msg",
  data: { text: "hi", userId: "u_123" },
});
```

The `.$messageTypes<ChatMessages>()` call is type-only, but it is enough for TypeScript to reject the wrong message type or payload shape. At runtime, the proxy appends messages to the channel's bounded replay window and fans them out to connected clients.

Reconnects use that same message id. SSE resumes with `Last-Event-ID`; WebSocket resumes with `last_message_id` in the query string or `lastMessageId` in the first auth frame. Browser clients keep reconnecting until explicitly closed, using bounded exponential backoff with jitter, and wake early when the browser reports it is online again. If the cursor has fallen out of the replay window, Tako returns `410 Gone` instead of pretending nothing was missed.

That is the part we care about most: params, auth, publish, replay, and reconnects are not five features you glue together. They are one channel definition, served by the same proxy that handles [deployment](/docs/deployment), [local HTTPS development](/docs/development), and the rest of your app's platform boundary. The [durable channels announcement](/blog/durable-channels-built-in) explains why Tako owns the socket; this is the TypeScript shape that makes it pleasant to use.
