---
title: "The Open Source Cloudflare Workers Alternative: Tako on Your Own VPS"
seoTitle: "Open Source Cloudflare Workers Alternative"
date: "2026-04-27T14:04"
description: "Same fetch handler model, same scale-to-zero feel — without V8 isolate constraints or per-request billing. Tako on your own VPS, with Cloudflare optional in front."
image: b058f40e042d
---

[Cloudflare Workers](https://workers.cloudflare.com/) is one of the most-loved deploy targets out there for a reason. V8 isolates that boot in under 5 ms. A `fetch` handler as the whole programming model. 200+ cities of presence, and a free tier generous enough to host a real project. We're fans.

It's also a hosted runtime with its own shape: a billing model based on requests and CPU-ms, and a JavaScript-isolate runtime that's deliberately a subset of Node. Tako brings the same fetch-handler model to a VPS you already pay for — same DX, different shape.

## Same export, different host

Both platforms speak the same language. A Worker exports `{ fetch }`; a [Tako app](/blog/the-fetch-handler-pattern/) exports `fetch` directly:

```typescript
export default function fetch(request: Request): Response {
  return new Response("Hello");
}
```

Same `Request` in, same `Response` out — the interface is web-standard. If you've shipped a Worker, you've already written a Tako app. The [Tako SDK](/docs/) handles the runtime bridging on Bun and Node.

## At a glance

|                          | **Cloudflare Workers (Standard)**                            | **Tako**                             |
| ------------------------ | ------------------------------------------------------------ | ------------------------------------ |
| **Runtime**              | V8 isolate                                                   | Native process (Bun, Node, Go)       |
| **Cold start**           | ~5 ms                                                        | Tens of ms (`fork()` + app init)     |
| **CPU time per request** | 30 s default, 5 min max                                      | Bounded by the box, not the platform |
| **Memory per request**   | 128 MB                                                       | Bounded by the box, not the platform |
| **Request body**         | 100 MB (Free/Pro)                                            | Configurable in the proxy            |
| **Subrequests per req**  | 10,000 paid (50 free)                                        | No platform-imposed limit            |
| **Script size**          | 10 MB gzipped (3 MB free)                                    | No platform-imposed limit            |
| **Runtime APIs**         | Partial Node — some modules are stubs                        | Full Node / Bun / Go                 |
| **Pricing**              | $5/mo + $0.30 / M requests + $0.02 / M CPU-ms over allowance | Your VPS bill                        |
| **Hardware**             | Cloudflare's global edge                                     | The VPS you rent (or own)            |
| **Lock-in**              | Workers platform                                             | None — it's your box                 |

## Where Workers shines

Workers' V8-isolate runtime is impressive engineering. Sub-5 ms cold starts, no container layer, every PoP warm — for a stateless API that fits inside a single isolate, it's hard to beat. The free tier (100k requests/day) makes it a great place for side projects, and the bindings (KV, R2, D1, Durable Objects, Queues, AI) plug straight into the same runtime.

The classic Workers wins are clear:

- **Edge logic in front of an origin.** Auth checks, rewrites, redirects, A/B bucketing, geo-routing. Run at every PoP, milliseconds from the user.
- **Lightweight JSON APIs and proxies.** Stateless request → response work where total CPU per request is comfortably under a second.
- **Workers AI inference.** Calling Cloudflare-hosted models from the same runtime, no separate infra.
- **Static site personalization.** Inject per-user content into HTML at the edge, with KV for the hot keys.

For workloads in that shape, Workers is excellent. Cloudflare earned that.

## Where Tako is different

### Full Node, Bun, Go

A Worker is a sandboxed JS runtime that implements a curated subset of Node — by design, since the goal is to fit inside a fast-starting isolate. That tradeoff works for a lot of apps, and not for others. If you reach for `child_process`, `cluster`, `vm`, or HTTP/2, those are stubs. Long-running CPU work and big memory footprints aren't really the target either.

A [Tako app](/docs/how-tako-works/) is a normal OS process, so the runtime is the whole runtime. Concretely, that means you can:

- **Shell out to native binaries.** `ffmpeg` for video transcoding, `imagemagick` for image processing, `pandoc` for document conversion, `git` for repo operations. `child_process.spawn` works the same way it does on your laptop.
- **Use embedded SQLite.** `bun:sqlite` or `better-sqlite3` — local, file-backed, fast, no network hop. Pair it with [`TAKO_DATA_DIR`](/docs/tako-toml/) for persistence across deploys.
- **Run heavy CPU work.** PDF generation, ML inference, image resizing, server-side rendering for long pages. No 30-second cap, no 128 MB ceiling.
- **Hold persistent connections.** WebSocket pub/sub, SSE streams, long polling — all just open file descriptors on a long-lived process.
- **Pick the runtime that fits.** Bun for speed and DX, Node for the ecosystem, Go for a single static binary. All first-class in [`tako.toml`](/docs/tako-toml/).

Whatever runs on your laptop runs on your server.

### A flat bill instead of a meter

Workers' bill scales with traffic: $5/month minimum, then $0.30 per million requests above the 10M monthly allowance, plus $0.02 per million CPU-ms above the 30M CPU-ms allowance. For light workloads that's genuinely cheap. For workloads with bursty traffic or heavier CPU per request, the math gets less predictable.

A worked example: a side project doing 50M requests a month at an average 30 ms of CPU per request lands at roughly **$5 + $12 + $29 ≈ $46/month** on Workers Standard. The same workload on a [Hetzner CX22 (4 GB RAM, ~$6/month)](https://www.hetzner.com/cloud) is well within budget — and the box has plenty of headroom for a second app, a staging environment, or a background worker on the same line item.

Tako's bill is your VPS bill — a flat number you already approved. If the box gets busy, provision a bigger one, or [add another server](/blog/build-your-own-edge-network-on-commodity-hardware/) and split traffic. Either way, no surprise CPU-ms invoice when something goes viral.

### Cloudflare can still sit in front

The "edge" part of Workers isn't the runtime — it's Cloudflare's anycast network. You can keep that part. Put Cloudflare DNS, TLS, and Argo smart routing in front of Tako servers in a few regions, and traffic still gets routed to the nearest healthy origin.

```d2
direction: right

users: Users {shape: cloud; style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
cf: Cloudflare\n(DNS + TLS + Argo) {shape: cloud; style.fill: "#9BC4B6"; style.font-size: 16}
la: LA VPS\n(Tako) {shape: hexagon; style.fill: "#E88783"; style.font-size: 14}
fra: Frankfurt VPS\n(Tako) {shape: hexagon; style.fill: "#E88783"; style.font-size: 14}
sgp: Singapore VPS\n(Tako) {shape: hexagon; style.fill: "#E88783"; style.font-size: 14}

users -> cf
cf -> la
cf -> fra
cf -> sgp
```

You get the edge story without renting the runtime. The full setup is in [Build Your Own Edge Network on Commodity Hardware](/blog/build-your-own-edge-network-on-commodity-hardware/).

### A platform, not just a runtime

Workers is a runtime; the platform around it (KV, R2, D1, Durable Objects, Queues, Workflows) lives behind separate bindings and separate line items. The pieces are excellent individually — they also each come with their own pricing dimension and their own architectural assumption. Stateful WebSocket fan-out via Durable Objects, for example, is a real shift in how you write the app.

Tako is heading toward the same primitives — [durable channels](/blog/durable-channels-built-in/), [workflows](/blog/durable-workflows-are-here/), queues, image optimization — built into the same `tako-server` binary that already routes your traffic. SQLite for storage, the local filesystem for blobs, in-process pub/sub for channels. One process per box, one [config](/docs/tako-toml/), one bill. Less power on day one than a fully-loaded Cloudflare account, but much less to learn before you ship.

## When each makes sense

Pick **Cloudflare Workers** if you want zero infrastructure, your code fits the V8-isolate model, and the bindings (KV, R2, D1, Durable Objects, AI) are doing real work for you.

Pick **Tako** if you want the same fetch-handler DX without the per-request meter, and you'd rather run on hardware you control with full Node, Bun, or Go APIs available. Stick Cloudflare in front if you still want the edge.

Both are reasonable. Same shape, different host.

[Get started with the docs →](/docs/)
