---
title: "How to Run a Full-Stack Next.js App on One $5 VPS: HTTPS, Jobs, WebSockets, and Images"
date: "2026-06-07T00:19"
description: "Deploy a full-stack Next.js app to one cheap VPS with Tako: HTTPS, rolling deploys, workflows, WebSocket/SSE channels, and image optimization."
image: cbd3c4bcfbec
---

One cheap VPS can run more than a homepage.

The interesting question is what happens after the first deploy. A real Next.js app wants HTTPS, static assets, API routes, background jobs, WebSocket or SSE updates, image optimization, secrets, logs, and a deploy path that does not turn every change into a tiny incident. You can bolt those together from separate tools. Or you can make the VPS run like a small app platform.

This is the Tako version: one server, one Next.js app, one [`tako.toml`](/docs/tako-toml/), and the full-stack pieces most apps reach for after week two. The deeper reference pages are the [Next.js framework guide](/docs/framework-guides/#nextjs), [deployment docs](/docs/deployment/), [development guide](/docs/development/), and [CLI reference](/docs/cli/).

## Start with the Next.js runtime

Next.js is portable because it can run as a Node.js server. The current Next.js docs describe `output: "standalone"` as a deployment mode that writes a minimal `.next/standalone/server.js`; running that server starts the production app. Tako wraps that path instead of inventing a separate Next runtime.

Install the SDK and wrap your config:

```bash
bun add tako.sh
```

```ts
// next.config.ts
import { withTako } from "tako.sh/nextjs";

export default withTako({
  images: {
    remotePatterns: [
      {
        protocol: "https",
        hostname: "cdn.example.com",
        pathname: "/uploads/**",
      },
    ],
  },
});
```

`withTako()` enables standalone output, installs the Tako adapter, adds `*.test` and `*.tako.test` to Next's allowed dev origins, writes `.next/tako-entry.mjs`, and configures `next/image` to use Tako's public optimizer. If Next emits standalone output, Tako uses it; otherwise the wrapper falls back to `next start`.

Then keep the deployment config small:

```toml
runtime = "bun"
preset = "nextjs"
app_root = "."

[envs.production]
routes = ["app.example.com"]
servers = ["vps-1"]

[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
formats = ["webp"]
```

`app_root = "."` is useful for root-level Next projects where `channels/`, `workflows/`, `instrumentation.ts`, and `tako.d.ts` live next to `next.config.ts`. If you keep backend definitions under `src/`, leave the default alone.

| Piece                 | Where it lives                     | What Tako does with it                                        |
| --------------------- | ---------------------------------- | ------------------------------------------------------------- |
| Next server           | `.next/tako-entry.mjs`             | Starts the standalone server or `next start`                  |
| Public/static assets  | `public/` and Next build output    | Serves matching files directly after route match              |
| Image optimizer       | `/_tako/image`                     | Validates sources, transforms with libvips, caches variants   |
| Durable channels      | `<app_root>/channels/*.ts`         | Serves WebSocket/SSE endpoints under `/_tako/channels/<name>` |
| Durable workflows     | `<app_root>/workflows/*.ts`        | Runs jobs in supervised worker processes with persisted state |
| Routes, TLS, rollouts | `[envs.production]` in `tako.toml` | Maps hostnames, manages certificates, and rolls new instances |

## Add jobs, WebSockets, and images

The one file Next.js apps need for backend primitives is `instrumentation.ts`. Next standalone runs routes in a child process, so the Tako runtime needs to initialize there before a route or server action enqueues a workflow or publishes a channel message.

```ts
// instrumentation.ts
export async function register() {
  if (process.env.NEXT_RUNTIME === "nodejs") {
    const { initServerRuntime } = await import("tako.sh/internal");
    initServerRuntime();
  }
}
```

Now add a workflow:

```ts
// workflows/send-receipt.ts
import { defineWorkflow } from "tako.sh";

export default defineWorkflow<{ orderId: string; email: string }>("send-receipt", {
  retries: 4,
  async handler(payload, ctx) {
    await ctx.run("email", async () => {
      ctx.logger.info("sending receipt", { orderId: payload.orderId });
      // send the email here
    });
  },
});
```

Workflows give you named step checkpoints, retries, cron schedules, sleeps, signals, and workers that scale to zero by default. They ship with the app on [`tako deploy`](/docs/deployment/), read the same secrets, and run next to the HTTP process without a separate Redis queue.

Then add a channel:

```ts
// channels/orders.ts
import { defineChannel } from "tako.sh";

export default defineChannel("orders", {
  auth: "public",
}).$messageTypes<{
  updated: { orderId: string; status: string };
}>();
```

Channels are served at `/_tako/channels/orders`. The proxy owns the WebSocket/SSE connection, stores published messages in a bounded replay log, and lets reconnecting clients catch up from a retained cursor. Use your product database for canonical history; use channels for live delivery and short reconnect replay.

Your Next route can now use both:

```ts
// app/api/orders/route.ts
import sendReceipt from "@/workflows/send-receipt";
import orders from "@/channels/orders";

export async function POST(req: Request) {
  const order = await req.json();

  await sendReceipt.enqueue({ orderId: order.id, email: order.email });
  await orders().publish({
    type: "updated",
    data: { orderId: order.id, status: "placed" },
  });

  return Response.json({ ok: true });
}
```

Images stay ordinary in React:

```tsx
import Image from "next/image";

export function ProductHero() {
  return <Image src="/images/product.jpg" width={1200} height={800} alt="Product" priority />;
}
```

Because `withTako()` configured the loader, the browser requests `/_tako/image?...` instead of sending image work through a custom app route. Local sources are allowed by default, remote sources must match `[images].remote_patterns`, and WebP is the default output format. The optimizer uses source and transform caches, so the first request for a size is the expensive one and repeated requests are cheap.

```d2
direction: right

browser: "Browser"
proxy: "tako-server\nTLS + routes"
next: "Next.js\npages + API routes"
jobs: "Workflow worker"
channels: "Channel replay\nWebSocket/SSE"
images: "Image optimizer\nlibvips + cache"

browser -> proxy: "HTTPS"
proxy -> next: "SSR / API"
next -> jobs: "enqueue"
next -> channels: "publish"
browser -> proxy: "/_tako/channels/orders"
browser -> proxy: "/_tako/image"
proxy -> channels: "connect + replay"
proxy -> images: "validate + transform"
```

## Deploy the whole app

Install the server once:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
tako servers add prod-a.tailnet.ts.net --install
```

Then deploy:

```bash
tako generate
tako deploy --env production
```

Deploy validates the production environment, routes, secrets, server metadata, image and channel/workflow requirements, then builds and packages the app. The server extracts the release under `/opt/tako/apps/{app}/{env}/releases/{version}/`, runs the runtime plugin's production install, starts a fresh instance, waits for health, adds it to the load balancer, and drains the old instance. Exact public routes use Let's Encrypt by default; wildcard routes can use Cloudflare DNS-01, and Cloudflare-proxied apps can use Cloudflare Origin CA.

For local development, run:

```bash
tako dev
```

You get local HTTPS, `.test` hostnames, watched channel/workflow definitions, generated TypeScript declarations, local image optimizer routes, and the same shape as production. If something feels off, `tako doctor` checks the daemon, DNS, proxy, CA trust, and repair hints.

The point is not that one $5 VPS should run every company. The point is that a small box can be a complete application environment for a surprising amount of work. With Tako, the cheap server is not just a place where `next start` happens. It is the boundary that handles HTTPS, rollouts, jobs, WebSockets/SSE, images, logs, secrets, and routing for the app you already wrote.

That gives you a practical default shape:

| App need                    | One-box answer                                       |
| --------------------------- | ---------------------------------------------------- |
| HTTPS and host routing      | Tako proxy, routes, certificates, redirects          |
| API routes and SSR          | Next.js standalone server behind the proxy           |
| Background work             | Durable workflow workers, idle when unused           |
| Live updates                | Durable channels with bounded reconnect replay       |
| Product and marketing media | `next/image` through Tako's optimizer and cache      |
| Operational loop            | `tako logs`, `tako status`, `tako releases rollback` |

When the app outgrows one machine, the same config model can add servers. Until then, the nice part is how little infrastructure vocabulary you need to introduce before shipping the full-stack version.

Start with the [quickstart](/docs/quickstart/), skim the [Next.js guide](/docs/framework-guides/#nextjs), or inspect the [open-source repo](https://github.com/tako-sh/tako). The box is small. The app does not have to be.
