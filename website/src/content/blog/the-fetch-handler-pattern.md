---
title: "The Fetch Handler Pattern: One Function, Every Runtime"
date: "2026-04-06T11:48"
description: "Why Tako chose the web-standard fetch handler as its universal app interface — and how the same export runs on Bun and Node."
image: 8bc5d8514b34
---

Here's a Tako app:

```typescript
export default function fetch(request: Request): Response {
  return new Response("Hello");
}
```

That's a complete, deployable application. Same file runs on Bun and Node.js. No framework, no adapter, no `createServer`. One function that takes a `Request` and returns a `Response` — both standard [Web APIs](https://developer.mozilla.org/en-US/docs/Web/API) that exist in every modern JavaScript runtime.

This is the interface Tako chose for everything. Your app is a fetch handler.

## Why not Express-style handlers?

Most Node.js frameworks invented their own request/response types before the web had standard ones. Express has `(req, res, next)`. Fastify has `(request, reply)`. Koa has `(ctx)`. Each one is a proprietary interface that locks your code to that framework's runtime model.

| Pattern       | Interface                                           | Portable?                                   |
| ------------- | --------------------------------------------------- | ------------------------------------------- |
| Express       | `(req: IncomingMessage, res: ServerResponse, next)` | Node.js only                                |
| Fastify       | `(request: FastifyRequest, reply: FastifyReply)`    | Node.js only                                |
| Koa           | `(ctx: Context)`                                    | Node.js only                                |
| **Web fetch** | `(request: Request) → Response`                     | **Bun, Node, Cloudflare Workers, browsers** |

The web fetch pattern won. Bun launched with `Bun.serve({ fetch })` as its primary API. Cloudflare Workers uses `export default { fetch }`. Frameworks like [Hono](https://hono.dev) and [Elysia](https://elysiajs.com) build on it natively — a Hono app is already a fetch handler, so it works with Tako out of the box:

```typescript
import { Hono } from "hono";

const app = new Hono();
app.get("/", (c) => c.text("Hello from Hono"));

export default app; // app.fetch is the handler
```

No adapter needed. No `toNodeHandler()`. The app _is_ the interface.

## The Node.js bridge

There's one catch: Node.js still doesn't have a native fetch-based HTTP server. `http.createServer()` gives you `IncomingMessage` and `ServerResponse` — the same callback shape from 2009.

The [Tako SDK](/docs) bridges this gap. When your app runs on Node, the SDK's entrypoint converts between the two worlds:

```d2
direction: right

node: Node HTTP Server {
  style.font-size: 13
}

bridge: SDK Bridge {
  shape: circle
  style.font-size: 13
}

handler: Your fetch() {
  shape: hexagon
}

node -> bridge: "IncomingMessage\nServerResponse"
bridge -> handler: "Request"
handler -> bridge: "Response"
```

Incoming: the SDK reads the Node request's URL, method, headers, and body stream, then constructs a standard `Request`. Outgoing: it takes your `Response`, writes the status and headers back through Node's `ServerResponse`, and pipes the body. About 60 lines of adapter code that you never see.

On Bun it's `Bun.serve({ fetch: handler })`. Node uses the SDK's small server bridge so your exported fetch handler keeps the same shape.

## What the SDK adds

Your fetch handler is your app's logic. The SDK wraps it with infrastructure concerns — things that happen _around_ your handler, not inside it:

```typescript
// What you write:
export default function fetch(request: Request): Response {
  return new Response("Hello");
}

// What actually runs (simplified):
function wrappedHandler(request: Request): Response {
  if (request.headers.get("host") === "tako") {
    return statusEndpoint(); // built-in health check
  }
  return yourFetchHandler(request, env);
}
```

The SDK reads [secrets from fd 3](/blog/secrets-without-env-files) before importing your code, intercepts internal health check requests, and signals readiness to the server. Your function stays clean — just `Request` in, `Response` out. The [Why Tako Ships an SDK](/blog/why-tako-ships-an-sdk) post covers this in more detail.

## Framework SSR works too

What about full-stack frameworks that aren't just API servers? TanStack Start, Nuxt, SolidStart — they all have SSR builds that produce a server entry.

Tako's [Vite plugin](/docs/presets) normalizes their output. After the framework builds, the plugin emits a thin wrapper that finds the fetch handler in the build output — whether it's a default export, a named `fetch` export, or a module with a `.fetch` method — and re-exports it in the shape Tako expects. Same pattern, same infrastructure, same [deploy flow](/docs/deployment).

## The portability argument

The fetch handler pattern isn't ours. It's the web platform's. If you ever move off Tako, your app is still a valid Bun server or Cloudflare Worker. Remove the SDK, add a small server binding, and you're done.

This matters because deploy tools come and go. ([RIP Waypoint, RIP Nginx Unit.](/blog/tako-vs-coolify)) The web `Request`/`Response` API is an IETF standard backed by every major runtime. Betting on it means your app code outlives whatever infrastructure runs it.

We think the best app interface is one you already know. If you've used `fetch()` to make an HTTP call, you already understand how to handle one.

Check out the [docs](/docs) to get started, or the [CLI reference](/docs/cli) for the full command set.
