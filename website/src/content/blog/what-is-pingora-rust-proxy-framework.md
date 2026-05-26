---
title: "What Is Pingora? The Rust Proxy Framework Behind Cloudflare and Tako"
seoTitle: "What Is Pingora? Rust Proxy Framework"
date: "2026-05-21T13:52"
description: "A plain-language guide to Pingora, Cloudflare's Rust proxy framework, and why Tako uses it to build programmable VPS app routing."
image: 92cd16dbe117
---

Pingora is easy to misunderstand if you meet it through a headline.

It is not "Nginx, but Rust." It is not a Caddyfile with different syntax. It is not Traefik with labels. [Pingora](https://github.com/cloudflare/pingora) is a Rust framework for building programmable network services, including HTTP proxies, load balancers, gateways, and custom traffic systems.

That distinction matters. A normal reverse proxy is something you configure. Pingora is something you build with.

For Tako, that is the whole point. We do not need a proxy that only knows how to forward `example.com` to `127.0.0.1:3000`. We need a proxy that can participate in deploys, route matching, TLS selection, static asset serving, app health, cold starts, and scale-to-zero. Pingora gives us the request machinery; Tako supplies the app platform around it.

This is the beginner version. For the lower-level request diagram, read [Cloudflare Pingora Architecture Diagram](/blog/cloudflare-pingora-architecture-diagram/). For the proxy selection story, read [Pingora vs Caddy vs Traefik](/blog/pingora-vs-caddy-vs-traefik/).

## Pingora in plain English

Pingora is a set of Rust crates that handle the hard parts of proxying and network services: accepting connections, running an async server, speaking HTTP, choosing upstreams, handling TLS, pooling connections, shutting down gracefully, collecting metrics, and letting your code hook into the request path.

The important phrase is "letting your code hook in."

| If you want...                             | Use a standalone proxy | Use Pingora                        |
| ------------------------------------------ | ---------------------- | ---------------------------------- |
| A config file in front of one app          | Yes                    | Probably too much work             |
| Automatic HTTPS with minimal setup         | Caddy is great         | You implement the certificate flow |
| Docker or Kubernetes service discovery     | Traefik is great       | You implement discovery            |
| A proxy embedded in your Rust platform     | Awkward                | This is the fit                    |
| Request routing based on app runtime state | External glue          | Put it in the proxy logic          |

In a traditional setup, the proxy is a separate process with its own config and lifecycle. Your deploy script edits config, reloads the proxy, starts processes somewhere else, and hopes every piece agrees.

With Pingora, the proxy can be part of your program. That program can keep a route table in memory, ask a process manager which app instances are healthy, serve one request directly, wake a sleeping app for the next request, and forward traffic only when an upstream is ready.

That is why it feels less like "install this proxy" and more like "build the edge you need."

## The core idea: a programmable request lifecycle

Pingora's HTTP proxy model is built around the `ProxyHttp` trait. The docs show it as a lifecycle: a new request enters, your code can inspect it, your code chooses an upstream peer, your code can adjust the request before it goes upstream, Pingora forwards it, your code can observe or adjust the response, and logging runs at the end.

Here is the shape without the Rust syntax:

```d2
direction: right

request: "incoming request" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

filter: "request_filter\ninspect or answer early" {
  style.fill: "#9BC4B6"
}

peer: "upstream_peer\nchoose backend" {
  style.fill: "#9BC4B6"
}

upstream: "upstream request\nrewrite headers" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

response: "response filters\nobserve or adjust" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

log: "logging\nmetrics and cleanup" {
  style.fill: "#E88783"
}

request -> filter
filter -> peer: "continue"
filter -> log: "direct response"
peer -> upstream
upstream -> response
response -> log
```

The required part is choosing the upstream. The rest of the hooks are where a platform becomes interesting.

A small gateway might use `request_filter` to block private paths and `upstream_peer` to choose between two backend pools. A feature-flag proxy might parse a header once, store the result in per-request context, then use that context later when picking an upstream. An app platform like Tako uses the same idea for app routing and lifecycle.

The key difference from a static config is that these decisions can be live Rust code. They can use data structures, async state, metrics, locks, caches, app registries, and whatever rules your platform owns.

## What Tako builds with Pingora

Tako users do not configure Pingora directly. They write app config:

```toml
name = "api"
runtime = "bun"

[envs.production]
routes = ["api.example.com", "example.com/api/*"]
servers = ["prod"]
```

When you run [`tako deploy`](/docs/deployment/), Tako uploads the app, prepares the runtime, registers routes, starts instances, waits for SDK readiness, probes health, and shifts traffic. `tako-server` then uses Pingora as the HTTP and HTTPS edge for that app state.

Inside Tako, Pingora is the listener and request engine. Tako adds the app decisions:

| Pingora provides          | Tako adds                                               |
| ------------------------- | ------------------------------------------------------- |
| HTTP and HTTPS listeners  | Route declarations from [`tako.toml`](/docs/tako-toml/) |
| Request lifecycle hooks   | App selection by host and path                          |
| Upstream peer selection   | Healthy native process instances                        |
| TLS integration points    | SNI certificate lookup and ACME management              |
| Metrics and logging hooks | App-scoped logs and request metrics                     |
| Proxy forwarding          | Static asset fast path, channels, images, cold starts   |

That last row is the part a config-only proxy cannot know by itself. If a request matches `example.com/api/*`, Tako checks the route table, looks for Tako-owned endpoints, serves static assets when possible, and only then resolves a backend process.

If the app has been scaled to zero, backend resolution can trigger a cold start. The first request becomes the leader, the process starts, the SDK reports its bound port over file descriptor 4, the server probes the status endpoint, and waiting requests continue once the instance is healthy. From Pingora's point of view, Tako is just choosing an upstream. From the user's point of view, a sleeping VPS app woke up on demand.

## Why Rust matters here

Pingora being Rust is not just a branding detail. It means the proxy can live in the same language and async runtime as the rest of `tako-server`.

Tako's server is Rust. The app registry, load balancer, TLS manager, cold-start manager, static asset handling, image optimizer integration, and Pingora proxy all live in one binary. There is no separate Go plugin, no generated Nginx config, and no sidecar process whose state has to be reconciled after a deploy.

That does not make Pingora the best tool for every job. It makes it a strong foundation when the proxy is not an accessory. If you are building infrastructure where routing decisions depend on application state, Rust code inside the proxy is simpler than shelling out to another product's admin API.

## When you should care about Pingora

If you are deploying one personal app and just need HTTPS, you probably do not need to care. Use Caddy, or use Tako and let Tako hide the proxy entirely.

You should care about Pingora when you want to build a proxy-like system rather than configure one:

| You are building...                | Why Pingora fits                              |
| ---------------------------------- | --------------------------------------------- |
| A custom API gateway               | Request hooks are first-class                 |
| A load balancer with unusual rules | Upstream selection is code                    |
| A deploy platform                  | Proxy decisions can see app lifecycle state   |
| A cache or traffic service         | Request and response filters are programmable |
| A Rust infrastructure daemon       | The proxy can be embedded in the same binary  |

That is the short answer to "what is Pingora?" It is Cloudflare's Rust framework for programmable network services. It gives you the proxy engine, but not the product opinion.

Tako is one product opinion built on top: a VPS app platform where [`tako.toml`](/docs/tako-toml/), [`tako deploy`](/docs/deployment/), TLS, routing, process health, scale-to-zero, and local dev through [`tako dev`](/docs/development/) all share the same control plane.

Pingora is the framework. Tako is what we built with it.
