---
title: "Self-Hosted Presence for React: Online Users and Typing Indicators"
date: "2026-10-25T04:22"
description: "Build private chat presence in React with Tako: trusted identities, isolated rooms, multiple tabs, typing indicators, and automatic connection expiry."
image: 29a5b0563541
---

A green dot looks simple until someone opens three tabs, switches rooms, and closes their laptop halfway through typing. Now “online” needs an identity, a room boundary, and a way to disappear without receiving a polite goodbye.

[Tako Presence](/docs/presence/) handles that connection lifecycle on your own servers. You define who can join a space; the React hook joins it, receives member snapshots, and updates connection metadata. We'll use it to build a private chat's online count and typing indicator, including the details that keep three tabs from becoming three people.

Our [earlier Channels comparison](/blog/self-hosted-pusher-alternative/) described presence as a missing primitive. It has since shipped as its own API. Here's how to use it.

## 1. Give each chat an authorized presence space

This tutorial assumes an existing Tako JavaScript app with React, a session token, and a helper that checks chat membership. Create one default export under `<app_root>/presence/`; with `app_root` set to `src`, use:

```ts
// src/presence/chat.ts
import { definePresence } from "tako.sh";
import { verifyToken, canAccessChat } from "../lib/auth";

export default definePresence("chat", {
  paramsSchema: (t) => t.Object({ chatId: t.String({ minLength: 1 }) }),
  maxConnectionLifetimeMs: 60_000,
  auth: {
    async verify({ header, params }) {
      const user = await verifyToken(header?.value);
      if (!user || !(await canAccessChat(user.id, params.chatId))) return false;
      return { subject: user.id };
    },
  },
});
```

`verifyToken` and `canAccessChat` are app-owned helpers. The first validates the credential; the second checks permission to enter this particular chat. Returning `{ subject: user.id }` attaches that trusted identity to the connection. The browser cannot choose its `subject` or server-assigned `connectionId`.

Omitting `auth` makes a space public. Returning `true` allows anonymous access, which isn't what we want for this example. Use a stable user ID from your verified session, not a user ID supplied in params or metadata.

Presence isolates membership by the space name and all validated params. Connections to `chat` with `{ chatId: "one" }` share a list; connections with `{ chatId: "two" }` get a different list. The permission check still matters: knowing a chat ID must not grant access. These are Presence semantics; ordinary [channel params](/docs/channels/#understand-channel-params) don't create isolated message streams.

```d2
direction: right

tabs: "Browser tabs" {style.fill: "#9BC4B6"}
auth: "Verify token\nand chat access" {style.fill: "#FFF9F4"}
one: "chatId: one\nMember snapshot" {style.fill: "#E88783"}
two: "chatId: two\nSeparate snapshot" {style.fill: "#9BC4B6"}

tabs -> auth: "connect with chatId"
auth -> one: "authorized for one"
auth -> two: "authorized for two"
```

Every authorized connection within a room sees the same list. Per-user hiding and blocked-user filtering inside that list aren't supported. If someone must not see a participant, filtering the rendered list won't enforce that privacy requirement; design the membership boundary accordingly.

Permissions are checked when connecting. The example's one-minute maximum connection lifetime forces periodic reconnection and reauthorization. Choose that interval for your app's permission model; revoking access doesn't continuously rerun the verifier on an already-open socket.

## 2. Count people and publish typing metadata

The hook returns connection entries. Two tabs from one person have different `connectionId` values and the same trusted `subject`. Deduplicate subjects for a people count, and use connection IDs when rendering individual sessions.

Here's a component with an idle timeout so focusing an input doesn't mean “typing forever”:

```tsx
"use client";

import { useEffect, useRef, useState } from "react";
import { usePresence } from "tako.sh/react";

type Props = { chatId: string; token: string; userId: string };

export function ChatPresence(props: Props) {
  return <RoomPresence key={`${props.chatId}:${props.userId}`} {...props} />;
}

function RoomPresence({ chatId, token, userId }: Props) {
  const [draft, setDraft] = useState("");
  const idle = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const typing = useRef(false);
  const { members, status, error, update } = usePresence("chat", {
    params: { chatId },
    authorization: token,
    data: { typing: false },
  });

  useEffect(() => () => clearTimeout(idle.current), []);

  function stopTyping() {
    clearTimeout(idle.current);
    if (typing.current) update({ typing: false });
    typing.current = false;
  }

  function changeDraft(text: string) {
    setDraft(text);
    if (!text) return stopTyping();
    if (!typing.current) update({ typing: true });
    typing.current = true;
    clearTimeout(idle.current);
    idle.current = setTimeout(stopTyping, 1500);
  }

  const onlineUsers = new Set(members.flatMap((m) => m.subject ? [m.subject] : []));
  const typingUsers = new Set(members.flatMap((m) =>
    m.subject && m.subject !== userId && m.data?.typing === true ? [m.subject] : []
  ));

  return (
    <section>
      <p>{status === "open" ? `${onlineUsers.size} people online` : "Connecting…"}</p>
      {error && <p role="alert">Presence unavailable. Check your connection or session.</p>}
      <label>
        Message
        <input
          value={draft}
          disabled={status !== "open"}
          onChange={(event) => changeDraft(event.target.value)}
          onBlur={stopTyping}
        />
      </label>
      <p role="status">
        {status === "open" && typingUsers.size > 0
          ? `${typingUsers.size} other participant(s) typing…`
          : ""}
      </p>
    </section>
  );
}
```

Pass `userId` from the current session; it only controls which typing indicators this UI hides. Server authorization establishes identity. The count includes you, and another participant appears once even if several of their tabs are typing. Closing one of their tabs leaves their other connections present.

The component key resets the draft and its idle timer when the chat or signed-in user changes. The timeout clears typing after 1.5 seconds without input; blur and an empty draft clear it immediately. Call `stopTyping()` from your send handler too. This example only implements the presence UI; saving and sending messages stays in your application.

`data` supplies initial metadata. Later changes use `update()`, which replaces the complete metadata object. If you add cursor or activity fields, include them in each replacement. Metadata must be JSON-serializable and at most 16 KiB; invalid updates set the hook's error and preserve the previous valid value.

Metadata is client-provided, including `typing`. Treat it as a UI hint. The strict `=== true` check avoids accepting arbitrary truthy values, but no metadata field should grant permissions or establish identity. Use the trusted `subject` for an authorized profile lookup if you want names instead of a count.

Mount this connection once per chat view. If the header, composer, and participant list all need it, put the hook in a provider and share its result. Calling it separately in each component creates extra connections.

## 3. Check disconnects, expiry, and deployment

The hook waits for authorization before joining and considers the connection open after its first member snapshot. It sends a heartbeat every 15 seconds, reconnects with bounded backoff, and rejoins with its latest metadata. On disconnect, it clears the member list, so render “Connecting…” rather than presenting an empty list as proof everyone left.

| Event | Presence behavior |
| --- | --- |
| Another tab opens | A separate connection entry joins the room |
| A connection closes cleanly | Its entry is removed; other tabs remain |
| Updates stop after network loss or a crash | Its lease expires 45 seconds after the last update |
| A connection reconnects | It receives a new connection ID and fresh snapshot |
| Authorization is rejected | Retries stop until connection options change |

Connected readers receive membership changes in approximately one second. A reconnect's old entry can remain until its lease expires, another reason to count unique subjects. The typing timeout is application behavior, separate from the connection lease: suspended browser timers or a lost connection can leave a typing hint visible until cleanup or expiry. Don't treat the indicator as an exact statement about someone's activity.

Use [`tako dev`](/docs/development/) to work on the UI, but note its current limitation: local Presence is permissive and doesn't run auth callbacks. Local member entries therefore won't establish the verified subjects this people count relies on. Test private-room rejection and trusted identities in a deployed test environment before relying on them.

In that environment, open two tabs with the same account and confirm the count stays at one. Join with a second authorized account, type, then stop. Try a different chat and an account without access. Finally, disconnect a tab and observe cleanup or lease expiry from another connected client. These checks exercise the boundaries the interface depends on.

Single-server production uses Tako-managed SQLite by default. For [multiple servers](/docs/deployment/), configure the shared `postgres_url` credential so connections on different servers see the same members. Presence is scoped to the deployed app and environment. Go and Rust backends can register spaces too; the same React hook manages browser membership.

Presence records expire and aren't replayed as durable channel events. Keep lasting messages in your database, use [Stores](/docs/stores/) for live data reads, and use Presence for who's connected now. The [full reference](/docs/presence/) and [source](https://github.com/tako-sh/tako) cover the remaining options. The green dot now has a lifecycle behind it.
