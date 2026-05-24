---
title: "Pingora Proxy Manager: Why Tako Is More Than a Reverse Proxy Dashboard"
date: "2026-05-21T14:02"
description: "A Pingora proxy manager handles routes. Tako coordinates deploys, TLS, readiness, process state, and scale-to-zero in one app control plane."
image: 9409ef186dd0
---

If you search for "Pingora proxy manager", you are probably looking for something reasonable: a way to run a Rust proxy, add hosts, point each host at an upstream, issue certificates, and reload config cleanly.

That category makes sense. [Pingora](https://github.com/cloudflare/pingora) is a framework, not a finished dashboard. Projects like [Pingora Proxy Manager](https://github.com/DDULDDUCK/pingora-proxy-manager) and [Pingap](https://github.com/vicanso/pingap) show the natural next layer: a Pingora-backed reverse proxy product with config, a web UI, SSL automation, upstreams, access control, and observability.

Tako is not trying to be that dashboard.

Tako uses Pingora, but the product boundary is different. We are not asking, "How do we make proxy config nicer to edit?" We are asking, "What if the proxy, deploy system, TLS manager, process supervisor, readiness protocol, logs, and scale-to-zero all agreed on the same app state?"

That is bigger than a reverse proxy dashboard, and it can feel unusual if you expected a screen full of upstream rows.

## A proxy manager manages traffic config

A reverse proxy manager is usually centered on resources like hosts, upstreams, certificates, middleware, and access rules. That is useful because raw proxy config can get tedious fast.

| Job                 | What a proxy manager usually owns               |
| ------------------- | ----------------------------------------------- |
| Route a domain      | Host, path, and upstream mapping                |
| Add HTTPS           | Certificate issuance, renewal, and storage      |
| Change a backend    | Update upstream address, weight, or timeout     |
| Protect an endpoint | IP allowlists, basic auth, rate limits, plugins |
| Observe traffic     | Access logs, response codes, upstream timing    |
| Apply changes       | Hot reload or graceful restart                  |

That is the right shape when your apps already exist somewhere else. Maybe Docker starts them, systemd starts them, or Kubernetes owns the service registry.

It usually does not know how that process was built, what version it is, whether it is the new release or old release, which secrets it received, or whether the next request should wake it from zero.

Tako chooses not to separate that lifecycle from the proxy.

## Tako treats proxying as app lifecycle

In Tako, routes live with the app:

```toml
name = "api"
runtime = "bun"

[envs.production]
routes = ["api.example.com", "example.com/api/*"]
servers = ["prod"]
```

That little block is not just proxy config. It participates in [`tako deploy`](/docs/deployment), certificate issuance, route conflict detection, runtime startup, static asset serving, load balancing, logs, and cold starts. The same app identity appears in [`tako.toml`](/docs/tako-toml), server state, the Pingora route table, TLS, and instance management.

Here is the shape:

```d2
direction: right

dashboard: "Proxy manager" {
  direction: down
  ui: "dashboard"
  config: "proxy config"
  proxy: "Pingora proxy"
  certs: "cert store"
  upstreams: "existing upstreams"

  ui -> config: "edit host"
  config -> proxy: "reload"
  certs -> proxy: "TLS"
  proxy -> upstreams: "forward"
}

tako: "Tako control plane" {
  direction: down
  cli: "tako deploy"
  server: "tako-server"
  state: "app state"
  pingora: "Pingora proxy"
  app: "native app process"

  cli -> server: "artifact + routes + env"
  server -> state: "version, scale, health"
  state -> pingora: "route + backend"
  server -> app: "spawn + readiness"
  pingora -> app: "loopback request"
}
```

The proxy manager path starts with config and ends at traffic. Tako starts earlier. It builds and uploads the release, runs production install, starts the new instance, waits for readiness, probes health, adds the backend to the load balancer, drains old instances, and then keeps serving through the same Pingora proxy.

The proxy does not need a user-facing "reload this host" button. The route changed because the app changed.

## Readiness is not a port field

In a dashboard-oriented proxy, an upstream is usually something you type or discover: `127.0.0.1:3000`, `api:8080`, a Docker service, a Kubernetes service, or a static list of servers.

In Tako, deployed app instances bind to `127.0.0.1` on an OS-assigned port. The app starts with `PORT=0`, then the SDK writes the actual bound port to file descriptor 4 once the server is listening. Only after that does `tako-server` route traffic to the loopback endpoint.

The source of truth is not "the user entered port 3000." It is "this exact process instance reported that it is listening, then passed health checks."

| Question                           | Dashboard proxy answer                   | Tako answer                                           |
| ---------------------------------- | ---------------------------------------- | ----------------------------------------------------- |
| What port should receive traffic?  | The configured upstream port             | The port the instance reported on fd 4                |
| Is the new release ready?          | Usually external health or manual timing | SDK readiness plus health probe                       |
| Can the old process stop?          | Another tool decides                     | Rolling update drains it after the new one is healthy |
| Where are startup errors recorded? | Process manager or app logs              | App-scoped logs in Tako                               |
| Who updates the route?             | Human, API, file watcher, or provider    | The deploy flow updates app state                     |

This is where Pingora being a framework matters. The [`pingora-proxy`](https://docs.rs/pingora-proxy/latest/pingora_proxy/) crate exposes programmable request phases through `ProxyHttp`: inspect a request, answer early, select an upstream, adjust the upstream request, inspect the response, and log.

For a normal proxy, `upstream_peer` means "pick one of the configured backends." For Tako, it means "resolve the app route, maybe serve a Tako-owned endpoint, maybe serve a static file, maybe wake the app, then pick a healthy native process."

## TLS belongs to the app route too

TLS is another place where "proxy config" and "app lifecycle" overlap.

When a route is deployed, Tako knows which hostnames belong to the app. That route set drives certificate management. Exact hostnames can use Let's Encrypt HTTP-01. Wildcard hostnames use Cloudflare DNS-01 credentials set up with `tako credentials set ssl.cloudflare --env <env>`. Private and local hostnames get self-signed certificates.

At request time, the TLS handshake happens before app routing. The browser sends SNI, and `tako-server` chooses the certificate by exact hostname first, wildcard fallback second, then a default self-signed certificate if no match exists yet.

A reverse proxy dashboard can manage certs too. The difference is ownership. In Tako, certificates are not a separate panel that happens to mention the same host. They are an effect of app routes and deploy validation. If a wildcard route needs provider credentials, deploy can fail before shipping an unreachable app.

## Scale-to-zero makes the proxy active

The biggest philosophical split is scale-to-zero.

If an always-on app has no healthy backend, the proxy should return `503`. Something is wrong. But if an app is intentionally scaled to zero with [`tako scale 0`](/docs/cli), "no backend" is not the end of the request. It is the beginning of a cold start.

Tako's backend resolution handles that path:

| State                             | What Tako does                   |
| --------------------------------- | -------------------------------- |
| Healthy instance exists           | Route through the load balancer  |
| No backend, desired instances > 0 | Return `503 Service Unavailable` |
| No backend, desired instances = 0 | Start one instance and wait      |
| Startup passes readiness          | Continue the waiting request     |
| Startup times out                 | Return `504 Gateway Timeout`     |
| Startup fails                     | Return `502 Bad Gateway`         |
| Too many waiters                  | Return `503 Service Unavailable` |

The first request becomes the leader that triggers the spawn. Later requests wait behind the same cold start, up to the configured queue limit. The app is still a normal native process; it just does not need to sit in memory all day.

This is not a feature a proxy dashboard can add by editing an upstream row. It requires the proxy to talk to the process supervisor, readiness protocol, app state store, load balancer, and logs. In Tako, those are already in the same `tako-server` process.

## So is Tako a Pingora proxy manager?

Sort of, but only if you squint. The proxy matters. You would miss it immediately. But it is not the whole product.

If you want to manually manage reverse proxy hosts, upstreams, TLS, and access rules, a Pingora-powered proxy manager is a good category to explore.

If you want a VPS app platform, the proxy is not the product. The product is the agreement between [`tako.toml`](/docs/tako-toml), [`tako deploy`](/docs/deployment), TLS, secrets, readiness, native process supervision, scale-to-zero, static assets, app logs, and local HTTPS through [`tako dev`](/docs/development).

That is why Tako does not expose Pingora as a dashboard full of knobs. Pingora is the request engine inside the platform. Tako is the control plane around it.

The distinction is small until something changes: a new route, a new release, a failed startup, a wildcard cert, or a scaled-to-zero admin app waking up after lunch. At that moment, the question is not "did the proxy reload config?" It is "does the whole app lifecycle know what traffic should do next?"

That is the layer Tako is building.
