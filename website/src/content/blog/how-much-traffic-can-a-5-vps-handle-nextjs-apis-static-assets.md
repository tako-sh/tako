---
title: "How Much Traffic Can a $5 VPS Handle for Next.js, APIs, and Static Assets?"
date: "2026-06-07T00:09"
description: "Use Tako's public proxy benchmark to build a practical traffic budget for Next.js pages, API routes, static files, and images on a small VPS."
image: bc07a8ec122f
---

A $5-ish VPS is not a toy server. The useful question is not "can a cheap box run my app?" It almost certainly can. The useful question is: **which part of the app consumes the box first?** A static asset request, a cached image variant, an API route that hits Postgres, and a server-rendered Next.js page all look like "web traffic" on a bill. They do not look the same to the CPU.

We now have enough public Tako data to stop hand-waving. The [small-VPS HTTPS proxy benchmark](/blog/tako-vs-envoy-caddy-haproxy-nginx-https-proxy-benchmarks/) put Tako on one 2 vCPU VM, with load generator, proxy, and upstream app sharing the machine. Tako returned 12,504 clean HTTPS 200 RPS at c5000 and 7,266 clean HTTPS 200 RPS at c20000, with zero non-200 responses and zero client errors in every Tako HTTP row.

That is not "your Next.js app will do 12k RPS." It is a ceiling for a very small upstream response on one saturated box. Real apps spend CPU on rendering, JSON, database calls, auth, image transforms, and cache misses. The benchmark is still useful because it gives the first budget line: on modest hardware, the platform path is unlikely to be the first limit for most small apps.

## Start with the traffic mix

A Next.js app on a VPS usually has at least three traffic classes:

| Traffic class            | What the server does                                                                 | Usual bottleneck                                      | Tako path                                                 |
| ------------------------ | ------------------------------------------------------------------------------------ | ----------------------------------------------------- | --------------------------------------------------------- |
| Static assets            | Serve files from `public/` or framework build output                                 | TLS, file I/O, connection count, browser cache policy | Direct static response after route match                  |
| Optimized images         | Validate source, load original, resize/encode misses, serve cached variants          | Transform misses first, cache hits later              | `/_tako/image` plus source and transform caches           |
| API routes and SSR pages | Run your Node/Bun process, app code, data fetching, auth, serialization, and headers | App CPU, database, external APIs, memory              | Proxy to a healthy app instance selected by `tako-server` |

That table matters more than one headline RPS number. If 80% of your traffic is static JS, CSS, fonts, and images that browsers cache aggressively, your server has a different life than an app where every request renders a dashboard and fans out to three APIs.

Next.js helps with this split. Its current deployment docs describe `output: "standalone"` as a way to produce a deployment-ready folder with the traced runtime files, and static export as a separate `output: "export"` mode for sites that do not need server features. With Tako, the normal server-rendered path is:

```ts
// next.config.ts
import { withTako } from "tako.sh/nextjs";

export default withTako({});
```

The [`nextjs` preset](/docs/presets/) and framework guide explain the rest: `withTako()` enables standalone output, installs the Tako adapter, configures local dev origins, and writes `.next/tako-entry.mjs`. If Next emits `.next/standalone/server.js`, Tako uses it; otherwise the wrapper falls back to `next start`. Your [`tako.toml`](/docs/tako-toml/) stays boring:

```toml
runtime = "bun"
preset = "nextjs"

[envs.production]
routes = ["app.example.com"]
servers = ["vps-1"]
```

The important part is where requests land. Tako matches the app route, reserves `/_tako/*` for platform endpoints, serves matching static assets directly, and proxies the rest to the app process.

```d2
direction: right

browser: "browser"
tako: "tako-server\nTLS + route match"
static: "static file\npublic/ or build assets"
image: "image optimizer\ncache hit or transform"
next: "Next.js process\nSSR + API routes"

browser -> tako: "HTTPS request"
tako -> static: "asset file exists"
tako -> image: "/_tako/image"
tako -> next: "page or API route"
static -> browser: "direct response"
image -> browser: "WebP/AVIF response"
next -> browser: "HTML or JSON"
```

## Translate the benchmark into a budget

The public proxy run gives one conservative starting point: a single small VM can move thousands of clean HTTPS responses per second through Tako's app-aware path when the upstream work is tiny. The run used a 2 vCPU, 7.8 GiB RAM VM, HTTP/1.1 over TLS, one certificate, one route, and a small Go plaintext app behind each proxy. Load generator, proxy, and app all ran on the same host.

Here is how to turn that into a practical Next.js budget without lying to yourself:

| Workload shape                                    | Capacity read from the benchmark                                               | What to measure in your app                                        |
| ------------------------------------------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------------ |
| Cached static assets and tiny API responses       | The platform can plausibly handle many thousands of clean HTTPS RPS on 2 vCPU  | File hit ratio, browser cache headers, compression, connection use |
| JSON APIs with normal database access             | The proxy is probably not the first limiter                                    | DB latency, pool size, p95 route time, serialization CPU           |
| SSR pages with auth, data fetching, and rendering | The benchmark is only a transport ceiling, not a page-rendering promise        | Render time, external calls, cache hit rate, memory per instance   |
| First-hit image variants                          | Do not compare to tiny-response RPS; transforms are intentionally bounded work | Miss rate, source size, output widths, transform queue pressure    |
| Cached image variants                             | Much closer to static traffic after the transform exists                       | Transform cache hit rate, disk cache size, response format         |

That gives a better answer than "12k RPS." For a marketing site with a few API calls, a cheap VPS may spend most of its time serving static files and cached image variants. For a SaaS dashboard, the app process and database will usually decide capacity first. For an image-heavy app, the first wave of uncached image sizes is the expensive part; after variants are cached, the same URLs become much cheaper.

Tako's [image behavior](/blog/self-hosted-nextjs-image-optimization-vps/) is designed around that distinction. Public optimized image responses use long-lived immutable cache headers. Source bytes are cached briefly in memory so one page asking for several widths can reuse the original, and transformed variants are cached on local disk under a bounded cache. Misses run through isolated libvips worker processes with a bounded queue, so image work does not silently eat the entire proxy and app budget.

This is also where [deployment behavior](/docs/deployment/) matters. During a rolling deploy, Tako starts a fresh instance, waits for the health check, adds it to the load balancer, and drains the old one. That does not make a slow page fast, but it keeps deploy mechanics from becoming the bottleneck.

## A concrete testing plan

If you want to know what your $5 VPS can handle, test traffic classes separately before you test the whole app. One combined "homepage" benchmark hides too much.

| Test target          | Example URL                                      | Why it exists                                     |
| -------------------- | ------------------------------------------------ | ------------------------------------------------- |
| Static asset         | `/assets/app.css` or `/_next/static/...`         | Measures direct file serving and TLS pressure     |
| Cached image variant | `/_tako/image?src=%2Fhero.jpg&w=1200` twice      | Separates transform miss from steady-state hit    |
| Cheap API route      | `/api/health` or `/api/version`                  | Measures proxy-to-app overhead with tiny app work |
| Real API route       | `/api/search?q=...`                              | Measures database, auth, and JSON work            |
| SSR page             | `/dashboard` with representative cookies/headers | Measures the page your users actually wait on     |

Run each target with enough concurrency to find the bend in the curve, not just the biggest RPS number you can screenshot. Watch p95 and p99 latency, not only average latency. Watch `tako logs` for app-scoped proxy diagnostics: request IDs, selected instances, route matches, cold-start wait time, upstream latency, compression fields, and handler/cache results. The [CLI reference](/docs/cli/) covers the log surface, and [How Tako Works](/docs/how-tako-works/) explains the proxy, app lifecycle, static files, images, and reserved routes.

That is how a $5 VPS becomes capacity planning.

## The honest answer

For tiny HTTPS responses through Tako's app-aware proxy path, the published small-VM baseline is thousands of clean 200 responses per second: 12.5k at c5000 and 7.3k at c20000 on the tested 2 vCPU VM. For static assets and warmed image variants, that is the right neighborhood to start thinking in. For API routes and server-rendered pages, your app work decides the number.

That is good news. The cheap VPS argument is not only about price; it is about having enough headroom to run the app and platform layer together. Tako handles routing, TLS, static files, image optimization, health checks, secrets, logs, and rolling deploys on the same machine, so the traffic question becomes concrete: what is static, what is cached, and what actually needs your Next.js process?

Start with the [Next.js framework guide](/docs/framework-guides/#nextjs), read the full [performance report](/performance/), and inspect the [open-source repo](https://github.com/tako-sh/tako) if you want to see the moving parts. Your small server probably has more room than you think. The trick is measuring the right room.
