---
title: "Cloudflare Pingora Architecture Diagram: How Tako Routes Requests, TLS, and Cold Starts"
seoTitle: "Cloudflare Pingora Architecture Diagram"
date: "2026-05-21T13:31"
description: "A request-path diagram of Tako's Pingora proxy: SNI, route matching, static assets, cold starts, load balancing, and upstream proxying."
image: 674ebe7d7a9b
---

Pingora is not a config file with a proxy hiding behind it. It is a Rust framework for building programmable network services, which is exactly why we use it in Tako.

In [our Pingora vs Caddy vs Traefik post](/blog/pingora-vs-caddy-vs-traefik/), we talked about the decision. This post is the wiring diagram: what actually happens when a browser request hits a Tako server, how TLS is selected, where route matching happens, when the proxy serves a file directly, and how a scaled-to-zero app wakes up before the request is forwarded.

If you came here looking for a Cloudflare Pingora architecture diagram, this is not Cloudflare's internal edge. This is Tako's edge path, built on the same programmable proxy framework.

## The request path in one diagram

Pingora's [`ProxyHttp` lifecycle](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/internals.md) is built around hooks: inspect the request, choose an upstream peer, adjust the upstream request, observe the response, and log the result. Tako uses those hooks as the edge control plane for deployed apps.

```d2
direction: right

browser: Browser {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

edge: "Pingora listener\n:80 / :443" {
  style.fill: "#9BC4B6"
}

tls: "TLS + SNI\ncertificate lookup" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

request_filter: "request_filter\nhost + path" {
  style.fill: "#9BC4B6"
}

routes: "Tako route table" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

fast_path: "Tako-owned endpoints\nand static assets" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

backend: "backend resolution" {
  style.fill: "#9BC4B6"
}

cold: "cold start gate\nif scaled to zero" {
  style.fill: "#E88783"
}

lb: "round-robin\nhealthy instance" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

upstream: "upstream_peer\n127.0.0.1:port" {
  style.fill: "#9BC4B6"
}

app: "app process\nSDK ready on fd 4" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

browser -> edge: "HTTP request"
edge -> tls: "HTTPS handshake"
tls -> request_filter: "request headers"
request_filter -> routes: "match host + path"
routes -> fast_path: "/_tako/* or file asset"
fast_path -> browser: "direct response"
routes -> backend: "dynamic request"
backend -> cold: "no healthy instance"
cold -> app: "spawn + wait"
app -> lb: "healthy"
backend -> lb: "ready backend"
lb -> upstream: "selected endpoint"
upstream -> app: "proxied HTTP"
app -> browser: "response"
```

The important part is where the decisions live. Tako does not generate an Nginx config, reload an external proxy, and hope the process manager agrees. The Pingora proxy, route table, load balancer, TLS manager, static file path, cold-start manager, and app process state all live in the same server process.

That is why features like [zero-downtime deploys](/blog/zero-downtime-deploys-without-a-container-in-sight/), [scale-to-zero](/docs/deployment/), and [multiple apps on one VPS](/blog/how-to-host-multiple-apps-on-one-vps-with-automatic-https/) are not separate layers. They are all request-path decisions.

## TLS happens before app routing

The first choice is not "which app gets this request?" It is "which certificate should the listener present?"

For HTTPS, the browser sends SNI during the TLS handshake. Tako's TLS layer asks the certificate manager for an exact hostname match first, then tries a wildcard certificate match, then falls back to a default self-signed certificate when no matching certificate exists yet. That fallback lets the TLS handshake complete so the HTTP layer can return a normal status like `404` for an unknown host.

Certificate behavior is tied to routes in [`tako.toml`](/docs/tako-toml/):

| Route shape          | Certificate behavior                            |
| -------------------- | ----------------------------------------------- |
| `example.com`        | ACME HTTP-01 certificate for the exact hostname |
| `api.example.com`    | ACME HTTP-01 certificate for the exact hostname |
| `example.com/docs/*` | Uses the `example.com` certificate              |
| `*.example.com`      | Wildcard certificate through Cloudflare DNS-01  |

HTTP requests normally redirect to HTTPS with a `307`, except for `/.well-known/acme-challenge/*`, which the proxy handles directly so Let's Encrypt can verify the domain. Wildcard routes are the special case: they require Cloudflare DNS-01 credentials because HTTP-01 cannot prove control of every possible subdomain. The deploy flow checks that before shipping the app.

So by the time Tako starts thinking about apps, TLS is already settled. The request is inside Pingora's HTTP lifecycle with a host, path, headers, and a per-request context.

## Route matching is pure host and path

Tako's route table is intentionally simple. Deployed apps contribute environment-level routes, and incoming requests are matched by hostname and path. The most specific match wins: exact host beats wildcard host, and longer path beats shorter path.

```toml
name = "docs"
runtime = "bun"

[envs.production]
routes = [
  "example.com/docs/*",
  "docs.example.com"
]
servers = ["prod"]
```

The route table accepts four useful shapes:

| Route                   | What it matches                    |
| ----------------------- | ---------------------------------- |
| `api.example.com`       | Exact hostname                     |
| `*.example.com`         | Any one matching subdomain         |
| `example.com/api/*`     | Hostname plus path prefix          |
| `*.example.com/admin/*` | Wildcard hostname plus path prefix |

Once `request_filter` has the selected app, it handles edge-owned responses before forwarding to an app process.

First, `/_tako/*` is reserved for Tako's public endpoints. Durable channels, image optimization, and signed storage URLs live there. Those requests are not ordinary app routes.

Second, static assets get a fast path. If a request looks like a file, Tako checks the deployed app's `public/` directory and serves the file directly when present. For path-prefixed routes, it also tries the prefix-stripped asset path. That is why an app mounted at `example.com/docs/*` can still serve a built asset like `/assets/main.js` when the browser asks for `/docs/assets/main.js`.

Only after those checks does the proxy need a backend process.

## Cold starts are backend resolution

Backend resolution asks a narrow question: is there a healthy instance for this app?

If yes, Tako selects one. If not, the answer depends on the app's desired instance count. For an always-on app, no healthy backend is an outage, so production returns a generic `503 Service Unavailable` and logs the app-scoped diagnostic. For an app that has been scaled to zero, no healthy backend is the wake-up path.

The cold-start manager uses a leader/waiter pattern:

| Situation                                  | Proxy behavior                              |
| ------------------------------------------ | ------------------------------------------- |
| First request arrives while app is at zero | Becomes the leader and starts one instance  |
| More requests arrive during startup        | Wait behind the same cold start             |
| Startup succeeds                           | Waiters continue to the new healthy backend |
| Startup exceeds 30 seconds                 | Return `504 Gateway Timeout`                |
| Spawn or readiness fails                   | Return `502 Bad Gateway`                    |
| More than 1000 requests wait               | Return `503 Service Unavailable`            |

The app process itself is still a normal native process. Tako sets `HOST=127.0.0.1` and `PORT=0`, then the [SDK](/docs/) binds an OS-assigned port and reports it back over file descriptor 4. The server probes the SDK status endpoint, marks the instance healthy, records cold-start metrics, and releases the waiting requests.

There is no container image to unpack and no external proxy config to rewrite. The request that discovered the app was cold is the same request that waits for the instance to become routable.

## Upstream proxying is the last step

Once a backend is ready, Tako's load balancer chooses a healthy instance with round-robin selection. The selected instance has a loopback endpoint like `127.0.0.1:47831`, and Pingora's `upstream_peer` hook turns that into the actual upstream connection.

Before forwarding, Tako adjusts the upstream request:

| Header or field         | What Tako does                                        |
| ----------------------- | ----------------------------------------------------- |
| `X-Forwarded-Proto`     | Sets `https` or `http` based on the effective request |
| `X-Forwarded-For`       | Sets the resolved client IP when trusted              |
| `Forwarded`             | Removes it before proxying                            |
| `X-Tako-Internal-Token` | Removes client-supplied values                        |
| Request body            | Enforces the configured body-size limit               |

When response headers arrive, Tako records upstream timing. When the request finishes, it releases per-IP rate-limit accounting, marks the selected instance request as finished, records end-to-end request metrics, and logs the final status.

That is the full shape: SNI first, route table second, Tako-owned endpoints and static assets before app traffic, cold-start backend resolution when needed, then loopback proxying to a healthy native process.

## Why this architecture matters

A standalone reverse proxy is great when routing is the whole job. Tako's proxy has a different job. It needs to know whether a deploy is rolling out, whether an instance is healthy, whether a route belongs to a static asset, whether a request should wake a sleeping app, and whether a wildcard hostname needs DNS-01 certificate handling.

Pingora gives us the programmable request lifecycle for that. Tako supplies the app model around it: [`tako.toml`](/docs/tako-toml/), [`tako deploy`](/docs/deployment/), local HTTPS in [`tako dev`](/docs/development/), encrypted secrets, runtime readiness, workflows, channels, and image optimization.

The result is a small edge inside your VPS: one Rust server process that terminates TLS, routes requests, manages app instances, and keeps the proxy aware of the deployment state it is serving.
