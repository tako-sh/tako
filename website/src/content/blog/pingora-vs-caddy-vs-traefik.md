---
title: "Pingora vs Caddy vs Traefik: Why We Built on Cloudflare's Proxy"
seoTitle: "Pingora vs Caddy vs Traefik"
date: "2026-04-04T14:13"
description: "How we chose Pingora over Caddy and Traefik for Tako's proxy layer — and what it means for performance, memory, and architecture."
image: e8b2740ac234
---

When you build a deployment platform, the reverse proxy is one of the first decisions you make — and one of the hardest to change later. Every request your users serve flows through it. It terminates TLS, routes traffic, handles health checks, and manages connections to your app processes.

We chose [Pingora](https://github.com/cloudflare/pingora), Cloudflare's Rust proxy framework. Here's why — and what the alternatives look like.

## The Three Contenders

The modern reverse proxy landscape has three serious open-source options worth considering:

|                       | Pingora                  | Caddy                       | Traefik                      |
| --------------------- | ------------------------ | --------------------------- | ---------------------------- |
| **Language**          | Rust                     | Go                          | Go                           |
| **GitHub stars**      | ~26k                     | ~71k                        | ~62k                         |
| **Architecture**      | Library/framework        | Standalone server           | Standalone server            |
| **Auto HTTPS**        | You implement it         | Built-in (flagship feature) | Built-in (Let's Encrypt)     |
| **Service discovery** | You implement it         | Plugin-based                | Native (Docker, K8s, Consul) |
| **Config**            | Code (Rust)              | Caddyfile / JSON API        | YAML / labels / API          |
| **Used by**           | Cloudflare (1T+ req/day) | Stripe, Tailscale           | IBM Cloud, SUSE, OVHcloud    |

Each is excellent. The choice depends on what you're building.

## Why Not Caddy?

Caddy is the easiest proxy to love. Automatic HTTPS out of the box, a clean config format, and a single static binary. If you need a reverse proxy today and don't want to think about it, Caddy is probably the right answer.

But Caddy is a _server_, not a _library_. You configure it — you don't program it. For Tako, we needed to deeply integrate the proxy with our [app lifecycle](/docs/how-tako-works/): spawning instances, waiting for [SDK readiness signals](/blog/why-tako-ships-an-sdk/), managing rolling updates, routing to healthy backends, and handling scale-to-zero cold starts. All of that requires tight coupling between the proxy layer and our process manager.

With Caddy, we'd be shelling out to an API or writing a plugin in Go alongside our Rust codebase. With Pingora, the proxy _is_ our code — same language, same async runtime, same memory space.

## Why Not Traefik?

Traefik's superpower is automatic service discovery. Point it at Docker or Kubernetes and it configures itself from container labels. That's genuinely impressive — 3.4 billion Docker Hub downloads impressive.

But Tako [doesn't use Docker](/blog/why-we-dont-default-to-docker/). We run apps as native processes. Traefik's auto-discovery doesn't help when your "services" are Bun processes on localhost ports that Tako's process manager controls directly. We'd be paying Traefik's memory overhead (50-200 MB) for features we'd never use, while writing our own routing layer on top anyway.

## Why Pingora

Pingora is different from Caddy and Traefik in a fundamental way: it's a _framework_, not a finished product. You write Rust code that implements Pingora's `ProxyHttp` trait, and you get a proxy that does exactly what you need.

That sounds like more work. It is. But it's the right tradeoff for a deployment platform.

**Performance at the foundation.** Cloudflare built Pingora to replace Nginx across their network — over a trillion requests per day. Their published numbers: 70% less CPU and 67% less memory compared to their previous Nginx-based service, with 80ms less TTFB at p95. Those gains come from Pingora's multithreaded architecture with shared connection pools — no more per-worker silos.

**Same language, same runtime.** Tako's server is Rust + Tokio. Pingora is Rust + Tokio. Our proxy, process manager, load balancer, TLS handler, and [cold-start manager](/docs/deployment/) all share one async runtime and one address space. No IPC, no serialization boundaries, no separate process to manage.

**Full control over the request path.** When a request arrives for a scale-to-zero app, Tako needs to: check if an instance is running, start one if not, wait for the SDK readiness signal, probe the health endpoint, add it to the load balancer, then forward the request. That's not something you configure — it's something you program. Pingora's trait-based API makes each of those steps a method you implement.

```d2
direction: down

request: Incoming request
proxy: Pingora Proxy
route: Route Table
lb: Load Balancer
app: App Process

cold: Cold start path {
  gate: Need instance?
  spawn: Spawn process
  ready: "TAKO:READY:port"

  gate -> spawn -> ready
}

request -> proxy: incoming
proxy -> route: lookup host
route -> cold.gate: app + backend
cold.ready -> lb: add backend
proxy -> lb: pick instance
lb -> app: 127.0.0.1:port
```

**Built-in caching.** Pingora ships with `pingora-cache` — an in-memory response cache with LRU eviction and cache-lock support for collapsing concurrent misses. We get upstream response caching without bolting on Varnish or writing our own.

## The Tradeoff

Pingora is not the easy choice. There's no Caddyfile equivalent, no dashboard like Traefik's, no plugin marketplace. You write Rust, you implement the traits, you handle ACME yourself. Our [TLS and certificate management](/docs/deployment/) is custom code using `instant-acme` and OpenSSL callbacks.

For a standalone reverse proxy, that's a bad deal. For a deployment platform where the proxy is one component in a tightly integrated system? It's exactly right.

## Choosing Your Proxy

If you need a quick reverse proxy for your existing services, use Caddy. If you're running containers and want zero-config service discovery, use Traefik. If you're building infrastructure where the proxy is a core component — not a bolt-on — Pingora gives you the foundation to build something fast and precise.

We went with Pingora because Tako isn't just a proxy in front of your app. It's a [complete platform](/docs/) — deployment, process management, routing, TLS, secrets, scaling — and the proxy needs to be a first-class participant in all of it.

Check out the [docs](/docs/) to see how it all fits together, or read about [how Tako works](/docs/how-tako-works/) under the hood.
