---
title: "Self-Hosted Live Queries for React: Introducing Tako Stores"
date: "2026-10-25T04:17"
description: "Keep React synchronized with your existing database using Tako Stores: authenticated live reads, explicit invalidation, and optional offline caching."
image: 62991c3db48d
---

You save a message. The database has it. The person in the next browser tab is still looking at yesterday's conversation. Somewhere between “write succeeded” and “everyone can see it,” you need subscriptions, permission checks, reconnect handling, and a way to ask for fresh data.

We're introducing [Tako Stores](/docs/stores/): self-hosted live queries for React, backed by the database you already use. Define an authenticated loader, read it with `useStore`, and invalidate it after a write. Tako connects those pieces and reloads the view when notified. You can also opt into cached reads for when the connection disappears.

“Live” has a specific meaning here: your application announces changes explicitly. Stores don't inspect SQL dependencies or automatically observe database writes. That keeps the integration in your application code, where you already know what changed and who may read it.

## Your database, an authorized loader, a React hook

A Store starts with a backend definition under `<app_root>/stores/`. For an app whose root is `src`, a chat's message list could look like this:

```ts
// src/stores/messages.ts
import { defineStore } from "tako.sh";
import { db } from "../lib/db";
import { verifyToken, canAccessChat } from "../lib/auth";

export default defineStore("messages", {
  paramsSchema: (t) => t.Object({ chatId: t.String({ minLength: 1 }) }),
  auth: {
    async verify({ header, params }) {
      const user = await verifyToken(header?.value);
      if (!user || !(await canAccessChat(user.id, params.chatId))) return false;
      return { subject: user.id };
    },
  },
  async load({ params }) {
    const rows = await db.message.findMany({
      where: { chatId: params.chatId },
      orderBy: { createdAt: "asc" },
    });
    return rows.map((row) => ({ id: row.id, text: row.text }));
  },
});
```

The database and authentication helpers belong to your app. Here, `verifyToken` validates the authorization header and `canAccessChat` checks membership. Replace those helpers and the query with your existing code. The loader can read from a database or another service; it returns the view your UI needs.

Tako validates params and checks authorization independently for reads and subscriptions. Returning a trusted `subject` also makes that identity available to `load`, so a loader can apply per-reader filtering. Each authorized reader gets a separately loaded snapshot. The subscription never broadcasts those records to everyone watching the same chat.

This example deliberately configures authentication. Omit `auth`, and the Store is public. Cookie authentication is available too; the [Stores reference](/docs/stores/) covers cookie-only and custom-header setups.

After adding the definition, run [`tako generate`](/docs/cli/) to refresh the generated Store types. React imports the hook, without importing backend code:

```tsx
"use client";

import { useStore } from "tako.sh/react";

export function Messages({ chatId, token }: { chatId: string; token: string }) {
  const { data, status, isStale, error, refresh } = useStore("messages", {
    params: { chatId },
    authorization: token,
  });

  if (data === undefined) {
    return <p>{error ? "Messages are unavailable." : "Loading messages…"}</p>;
  }

  return (
    <>
      {isStale && <p>Messages may be out of date; waiting for an update.</p>}
      <ul>
        {data.map((message) => (
          <li key={message.id}>{message.text}</li>
        ))}
      </ul>
      <button disabled={status === "loading"} onClick={() => void refresh()}>
        Refresh
      </button>
    </>
  );
}
```

Generated types connect the Store name to its params and serialized result. Returning explicit response objects keeps that shape easy to understand. The hook exposes the latest data, connection-related status, staleness, errors, and manual refresh. Your component decides how those states should feel to a reader.

Each hook owns its subscription. If several components need the same view, put the hook in a provider and share its result. JavaScript isn't required on the backend either: Go and Rust have Store registration APIs that work with the same browser client and React hook.

## Commit the write, then announce the change

The write path stays in your existing handler. Once its database transaction has committed, invalidate the Store with the affected params:

```ts
import messages from "../stores/messages";

await db.message.create({ data: { chatId, text } });
await messages.invalidate({ chatId });
```

That wakes readers of `messages` with those exact params. Each read goes through authorization again and loads current data. Invalidating one chat doesn't refresh another, and there is no wildcard invalidation. An unparameterized Store uses `invalidate()`.

Underneath, Stores use [Tako Channels](/docs/channels/) for notifications over an SSE subscription. The browser receives a small invalidation signal, then requests a fresh snapshot:

```d2
direction: right

write: "App write" {style.fill: "#FFF9F4"}
db: "Your database" {shape: cylinder; style.fill: "#9BC4B6"}
store: "Tako Store" {style.fill: "#E88783"}
react: "React reader" {style.fill: "#9BC4B6"}

write -> db: "1. commit"
write -> store: "2. invalidate(params)"
store -> react: "3. notify"
react -> store: "4. authorized read"
store -> db: "5. load current data"
store -> react: "6. private snapshot"
```

The notification contains no records, credentials, or raw params. Keeping data in the read path means two people subscribed to the same params can still receive different results according to their permissions and loader logic.

Reconnects matter just as much as the happy path. The client reloads after its stream is established and after it reconnects. If another invalidation arrives during a load, it follows up with another read. Several notifications can be coalesced: the goal is a current view, so the client doesn't promise to replay every intermediate database state.

There is an application responsibility here. Every writer that should update an open UI needs to invalidate the affected Store, whether that writer is a route, a workflow, or a separate script. A direct database edit won't automatically produce a notification.

The database commit and notification publication are also separate operations. If publication fails, `invalidate` reports the failure; the committed write is neither rolled back nor retried by Stores. Where eventual notification must survive that gap, use a transactional application outbox: record the notification intent alongside the write, then publish it from your delivery process. Reconnect catch-up and manual refresh can fetch current data, but they don't make those two operations atomic.

## Let saved reads survive a lost connection

A connection dropping shouldn't always mean an empty screen. For data appropriate to keep on a device, enable offline persistence on both sides. Add this policy to the backend definition:

```ts
offline: { maxAgeMs: 60 * 60 * 1000, version: "1" },
```

Then add a stable, non-secret identity scope to the hook options:

```ts
const { data, status, isStale } = useStore("messages", {
  params: { chatId },
  authorization: token,
  cacheScope: `${tenantId}:${userId}`,
  cacheVersion: "1",
});
```

Both choices are required. Without a backend policy or a client scope, results aren't persisted. The default browser cache uses IndexedDB and separates entries by origin, Store, params, scope, and version. Authorization credentials aren't stored. Change the scope when switching accounts or tenants, and bump both versions after an incompatible response-shape change.

| Situation                                      | What the reader can expect                              |
| ---------------------------------------------- | ------------------------------------------------------- |
| Connection lost, valid cached result available | Previously loaded data can remain visible, marked stale |
| Cached result expired or version mismatched    | That cached result is ignored                           |
| Read or subscription denied authorization      | The active result and its persisted entry are cleared   |
| Browser storage unavailable                    | Online reads still work                                 |

Clear the matching scope during logout and disable or unmount that session's readers:

```ts
import { clearStoreCache } from "tako.sh/client";

await clearStoreCache(`${tenantId}:${userId}`);
```

Offline reads are previously authorized data saved on that device. A disconnected browser cannot discover a remote permission revocation, so choose the data and expiry accordingly. The app shell must also already be available: Stores doesn't install a service worker. This release provides cached reads; queued writes, optimistic mutations, and conflict resolution aren't included.

You can develop this flow with [`tako dev`](/docs/development/), which watches JavaScript Store definitions and enforces Store authorization locally. Notification state is in memory during development and uses Tako-managed SQLite by default on a single production server. For multiple servers, configure the shared `postgres_url` credential so notifications can reach readers connected elsewhere. Your application database remains the source of truth.

Start with one view that's frustrating to keep fresh. Define its loader, connect the hook, and invalidate after its writes. The [Stores documentation](/docs/stores/) has the complete JavaScript, Go, and Rust examples; the [implementation is on GitHub](https://github.com/tako-sh/tako). Your database already knows what's new. Now your React UI has a way to find out.
