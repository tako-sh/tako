---
title: Presence
description: "Track live connections with authenticated identities, connection metadata, and a React hook."
---

Presence tracks who is connected to a named space. Tako manages membership, metadata updates, reconnects, and expiry. Each browser tab has its own entry, so closing one tab does not remove another tab belonging to the same user.

## Define A Space

Put one default export in each `<app_root>/presence/*.ts` file. As with channels, the first argument is the space name and `auth.verify` decides access.

```ts
// src/presence/lobby.ts
import { definePresence } from "tako.sh";
import { verifyToken } from "../lib/auth";

export default definePresence("lobby", {
  auth: {
    async verify({ header }) {
      const user = await verifyToken(header?.value);
      return user ? { subject: user.id } : false;
    },
  },
});
```

Omit `auth` or set it to `false` for public presence. Auth supports the same `headerName`, `cookieName`, and verification inputs as [channels](/channels/#authorize-and-choose-a-transport). A verification result of `true` allows an anonymous connection; `{ subject: "user-id" }` attaches a trusted identity. A client cannot set its subject or connection ID.

`paramsSchema` validates params before authorization. Presence membership is scoped by the space name and all validated params. For example, `params: { chatId: "one" }` and `params: { chatId: "two" }` have separate member lists. Auth must check that the user may access the requested chat. Authorized connections with the same params see each other. Channel and presence names have separate namespaces, so both can be named `lobby`.

## Guard Private Chat Presence

Authorize access on the server before returning any members:

```ts
// src/presence/chat.ts
import { definePresence } from "tako.sh";
import { verifyToken, canAccessChat } from "../lib/auth";

export default definePresence("chat", {
  paramsSchema: (t) => t.Object({ chatId: t.String({ minLength: 1 }) }),
  auth: {
    async verify({ header, params }) {
      const user = await verifyToken(header?.value);
      if (!user || !(await canAccessChat(user.id, params.chatId))) return false;
      return { subject: user.id };
    },
  },
});
```

```tsx
const { members, status } = usePresence("chat", {
  params: { chatId },
  authorization: token,
  data: { typing: false },
});

const isOtherUserPresent =
  status === "open" && members.some((member) => member.subject === otherUserId);
```

This remains true while any of that user's tabs are present in this chat. Every authorized connection in the chat sees the same member list. Per-user hiding and blocked-user filtering within that list are not currently supported. Permissions are checked when connecting; set `maxConnectionLifetimeMs` on the definition to bound the interval before a connection must reauthorize.

## Join From React

```tsx
"use client";

import { usePresence } from "tako.sh/react";

function Lobby({ token }: { token: string }) {
  const { members, connectionId, update, status, error } = usePresence("lobby", {
    authorization: token,
    data: { typing: false },
  });

  if (error) return <p>{error.message}</p>;

  return (
    <>
      <p>{members.length} connections online</p>
      <ul>
        {members.map((member) => (
          <li key={member.connectionId}>
            {member.subject ?? "Guest"}
            {member.connectionId === connectionId ? " (you)" : ""}
            {member.data?.typing ? " is typing" : ""}
          </li>
        ))}
      </ul>
      <input
        disabled={status !== "open"}
        onFocus={() => update({ typing: true })}
        onBlur={() => update({ typing: false })}
      />
    </>
  );
}
```

The hook returns:

| Field          | Meaning                                                                                  |
| -------------- | ---------------------------------------------------------------------------------------- |
| `members`      | Latest snapshot of `{ connectionId, subject?, data }` entries, including this connection |
| `connectionId` | This connection's server-assigned ID, or `null` while connecting                         |
| `status`       | `"connecting"` until the first snapshot; `"open"` while joined                           |
| `error`        | Latest connection, protocol, or metadata error, cleared on a successful snapshot         |
| `update(data)` | Replace this connection's complete metadata                                              |

`data` provides initial metadata. Use `update()` for later changes. Metadata must be JSON-serializable and at most 16 KiB. Invalid updates set `error` and preserve the last valid metadata. Metadata from other members is client-provided; only `subject` and `connectionId` are server-controlled. `params`, `authorization`, and `baseUrl` are passed as connection options, just as with channels.

Call the hook once in a provider and share its result when several components need the same presence connection. Each hook call creates another connection.

## Go Backends

The Go HTTP SDK is the root `tako.sh` module. Register spaces before calling
`tako.ListenAndServe`:

```go
import (
    "encoding/json"
    "tako.sh"
)

tako.RegisterPresence("chat", tako.PresenceDefinition{
    ParamsSchema: json.RawMessage(`{
        "type":"object",
        "properties":{"chatId":{"type":"string"}},
        "required":["chatId"]
    }`),
    Auth: &tako.ChannelAuthScheme{HeaderName: "authorization"},
    MaxConnectionLifetimeMs: 60_000,
    Verify: func(input tako.VerifyInput) tako.ChannelAuthDecision {
        userID, ok := authorizeChatPresence(input)
        if !ok { return tako.RejectChannel() }
        return tako.AllowChannel(tako.ChannelGrant{Subject: userID})
    },
})
```

`authorizeChatPresence` is your app's helper that verifies the credential and
access to the requested `chatId`. Cookie-only auth uses
`Auth: &tako.ChannelAuthScheme{CookieName: "session"}`. Omit `Auth` for public
presence. The existing HTTP adapter serves authenticated registry and auth
endpoints, and the React hook connects using the same public name and params.
`PresenceMember` and `PresenceSnapshot` expose wire types for custom consumers.
The separate `sdk/go` workflow-worker module does not need presence registration.

## Rust Backends

Enable the `axum` feature on the `tako` crate and register definitions explicitly:

```rust
use std::sync::Arc;
use serde_json::json;
use tako::{PresenceAuth, PresenceDefinition, PresenceGrant, PresenceRegistry};

let presence = Arc::new(PresenceRegistry::new());
presence.register("chat", PresenceDefinition::new()
    .params_schema(json!({
        "type": "object",
        "properties": {"chatId": {"type": "string"}},
        "required": ["chatId"]
    }))
    .max_connection_lifetime_ms(60_000)
    .auth(PresenceAuth::default(), |input| async move {
        authorize_chat_presence(&input).await.map(PresenceGrant::new)
    }));

tako::axum::serve_with_presence(app, presence).await?;
```

Your `authorize_chat_presence` helper returns `Some(user_id)` after verifying the
credential and chat access, or `None` to deny. Omit `.auth(...)` for a public
space. Cookie-only auth uses `PresenceAuth { header_name: None, cookie_name:
Some("session".into()) }`. For a custom Axum loop, wrap the app with
`tako::axum::router_with_presence(app, registry)`. The helpers protect internal
registry/auth endpoints with the bootstrap token and preserve status probes.
Other frameworks can call `registry.metadata()` and
`registry.authorize(input).await` behind the same internal protections.

Go and Rust provide backend registration and auth. The React hook manages
browser membership, metadata updates, heartbeats, and reconnects for all backends.

## Connection Lifecycle And Storage

The hook waits for authorization before joining. It sends a heartbeat every 15 seconds, reconnects with bounded backoff, and rejoins with the latest metadata. Authorization failures stop retries until connection options change. While disconnected, it clears the member list. Unmounting closes the connection.

Connected clients receive membership changes within approximately one second. Clean disconnects remove the entry; network loss, suspended tabs, and server crashes expire entries 45 seconds after their last update. A reconnect receives a new connection ID and a fresh snapshot. An old connection can remain visible until its lease expires.

Presence uses expiring records separate from durable channel replay. It does not replay old joins, leaves, or cursor positions. Use channels for durable live events and your app database for lasting records.

Local development keeps presence in memory and permits connections without running auth callbacks, matching local channel behavior. Single-server production uses Tako-managed SQLite by default. Multi-server deployments require the shared `postgres_url` credential, so connections on different servers see the same members. Presence state is scoped to the deployed app and environment. Configure `postgres_url` explicitly for multi-server Go and Rust apps; the deployment preflight currently detects JavaScript definition directories, not inline native registrations.
