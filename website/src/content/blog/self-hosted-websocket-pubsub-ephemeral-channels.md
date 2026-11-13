---
title: "Self-Hosted WebSocket Pub/Sub Without Message Persistence: Tako Ephemeral Channels"
seoTitle: "Self-Hosted WebSocket Pub/Sub: Ephemeral Channels"
date: "2026-11-02T03:15"
description: "Use Tako ephemeral channels for self-hosted WebSocket pub/sub: memory-only delivery, typed events, browser clients, and explicit reconnect limits."
image: 7fc33c508440
imageAlt: "An octopus broadcasts glowing message bubbles to connected laptops while a bubble bursts beside a disconnected laptop."
---

A browser is watching a live process. Another update arrives. The browser disconnects, and by the time it returns, those intermediate updates are already old news. Sometimes the useful thing to send is simply whatever happens next.

Tako Channels now support `ephemeral: true`: memory-only pub/sub over WebSocket or SSE. Events go to current subscribers locally and across connected native server peers. They never enter the channel replay log. For transient application signals, that gives you a delivery contract you can explain in one sentence: be connected when it happens.

## Choose what should disappear

[Durable channels](/blog/durable-channels-built-in/) remain the default. They store messages before delivery and keep a bounded replay window, so a reconnect can recover recent events. Ephemeral channels deliberately give up that recovery. The setting belongs to the channel definition; a browser cannot switch it by sending a connection option.

That distinction is useful when your product already knows how to recover its current state. A live progress display might load the authoritative job status from an application endpoint when opened, then show transient updates while connected. Missing an intermediate percentage is acceptable if reopening the page reads the current result.

Completion, payment, and audit records still belong in your app database. An ephemeral update saying “finished” can vanish during a disconnect. Your application must be able to discover the finished record independently. Even durable channel replay is a short delivery buffer rather than permanent product history.

| Behavior | Durable channel | Ephemeral channel |
| --- | --- | --- |
| Message storage | Stored before fanout | Events stay in process memory |
| Reconnect recovery | Replay within retained history | Future live events only |
| Message ID | Replay cursor | Empty string |
| Slow subscriber | Can use retained replay | Skips events evicted from the live buffer |
| Shared Postgres deployment | Supported | Rejected |

Each active ephemeral channel has a **64-message buffer**. It is a small bounded fanout buffer, not a queue for future subscribers. If no subscriber is connected, publishing does not save the event for later. When the last subscriber disconnects, the channel's buffer is released. A slow subscriber skips events that have already been evicted.

These are delivery guarantees, not a performance benchmark. Memory-only publishing avoids writing those events to the channel database, but we aren't promising a particular latency or throughput improvement. The [Channels reference](/docs/channels/#use-memory-only-channels) and [broadcast implementation](https://github.com/tako-sh/tako/blob/master/tako-channels/src/ephemeral.rs) document the boundary directly.

It also doesn't mean the whole application runs without a database. Tako can still manage its normal storage, and your app can still keep its own records. The promise is specifically that these channel events aren't persisted as channel messages or replay metadata. Choose the mode for its delivery semantics rather than treating it as a switch that removes every storage dependency from your deployment.

Use the higher-level primitives when they match the product. [Presence](/docs/presence/) manages online membership and current typing or cursor metadata. [Stores](/docs/stores/) provides authorized live reads of backend data. Ephemeral Channels are useful when you want your own transient event stream rather than those product-specific behaviors.

## Define one stream and connect the browsers

In an existing JavaScript or TypeScript Tako app, create a default-exported channel definition under `<app_root>/channels/`:

```ts
// <app_root>/channels/live-pulses.ts
import { defineChannel } from "tako.sh";
import { readOperatorSession } from "../lib/auth";

type PulseMessages = {
  pulse: { at: number };
};

export default defineChannel("live-pulses", {
  ephemeral: true,
  transport: "ws",
  auth: {
    async verify({ header }) {
      const session = await readOperatorSession(header);
      if (!session) return false;
      return { subject: session.userId };
    },
  },
}).$messageTypes<PulseMessages>();
```

`readOperatorSession` is application-owned authentication code: it must validate the supplied credential and verify that the user may access this shared operator stream. The helper receives Tako's parsed header value and returns your session or a falsy value. It isn't an SDK function. Omitting `auth` would make the channel public.

The explicit name, `live-pulses`, identifies the stream. Every authorized subscriber to that name receives the same broadcast. Channel params validate connection context; they do **not** isolate rooms, users, or tenants. Avoid adding `tenantId` to this example and assuming the resulting streams become private to each tenant. See [channel parameter semantics](/docs/channels/#understand-channel-params) before designing an access boundary.

`.$messageTypes<PulseMessages>()` types server-side publishing. From code running inside the Tako server runtime, import the handle and publish:

```ts
import livePulses from "../channels/live-pulses";

await livePulses.publish({
  type: "pulse",
  data: { at: Date.now() },
});
```

Use an import path appropriate to the calling module. The handle is already bound because the definition has no params. Browser code uses the browser client or React hook instead of this server-only publisher.

For a client-side React component:

```tsx
import { useChannel } from "tako.sh/react";

export function LivePulses({ token }: { token: string }) {
  const { messages, status, send } = useChannel<{ at: number }>(
    "live-pulses",
    { transport: "ws", authorization: token },
  );
  const latest = messages.at(-1);

  return (
    <div>
      <p>Connection: {status}</p>
      <p>Latest pulse: {latest?.data.at ?? "Waiting"}</p>
      <button
        disabled={status !== "open"}
        onClick={() => send({ type: "pulse", data: { at: Date.now() } })}
      >
        Send pulse
      </button>
    </div>
  );
}
```

The browser sends `{ type, data }` frames, and the proxy broadcasts them as supplied. The sender's timestamp is client-controlled data; don't interpret it as a trusted server clock or identity. Channel messages don't invoke an application mutation callback. Use normal authenticated HTTP endpoints when a request needs business validation or a database write.

```d2
direction: right

sender: "Connected browser"
server: "One Tako server process" {
  live: "live-pulses\nbounded memory buffer" {
    style.fill: "#9BC4B6"
  }
}
one: "Connected browser A"
two: "Connected browser B"

sender -> server.live: "WebSocket event"
server.live -> one: "live delivery"
server.live -> two: "live delivery"
```

WebSocket and persistence are separate choices. For server-to-browser updates only, omit `transport: "ws"` in the definition and use the default SSE subscription in the browser. `ephemeral: true` keeps the same memory-only semantics for either transport. Do not combine it with `replayWindowMs`; the definition rejects that combination, including an explicit zero replay window.

## Test the disconnect, then deploy within the limits

Run [`tako dev`](/docs/development/) and open two clients against the same channel. Send a pulse while both connections are open. Both clients should receive it. Close one client, send another pulse, and reopen the disconnected client. That missed pulse must not replay. Send a fresh pulse after reconnection and confirm it arrives.

This check is more useful than merely seeing two connected status labels. It establishes the behavior your UI must tolerate. A client that reconnects successfully can still have missed every event sent during its absence.

Ephemeral messages have `id: ""`, and SSE frames omit the replay ID field. The React hook reconnects WebSockets, but reconnecting does not restore missing ephemeral messages. Reload canonical application state as needed and resume watching future events. A server restart similarly discards process-local event state.

The hook's existing `messages` array is browser memory, separate from server replay. Seeing the last pulse still displayed after a connection breaks does not mean it was delivered again. Label that value as last observed, show the connection status, and decide when your UI should clear stale observations. Likewise, a resolved server-side `publish()` call isn't an acknowledgment that every browser processed an event. Channels broadcast; they don't claim work or collect subscriber acknowledgments.

Test authorization separately in a deployed test environment. Local channel connections bypass authorization callbacks, so a successful local subscription doesn't prove that `readOperatorSession` rejects unauthorized access in production. Verify an allowed operator and a denied client through the deployed route.

Production supports **one server or a native network of servers without a `postgres_url` credential**. Brokers forward live events to connected peers without storing messages or sequencing metadata. Disconnected peers miss events and resume with new ones after reconnection. Ordering is per originating broker, with no global order across concurrent publishers. A publish acknowledgement confirms local acceptance for best-effort fanout, not delivery to every subscriber. Shared Postgres stores still reject ephemeral publication and subscription, including on a single server.

Durable and ephemeral channels can coexist in the same supported app deployment. Keep durable events where replay matters, choose ephemeral events where loss is acceptable, and follow the [deployment guide](/docs/deployment/) when selecting servers and storage. For multiple servers, configure the native network and verify delivery between clients connected to different servers.

Start with a transient event whose absence your product can handle. Define its access policy, verify the disconnected case, and let the app's normal data API answer “what is true now?” The channel can concentrate on “what just happened while you were here?”
