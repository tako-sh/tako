---
title: "Scale-to-Zero Without Containers"
date: "2026-04-05T05:17"
description: "How Tako scales apps to zero and cold-starts them on demand — without Docker, containers, or a cloud platform."
image: d553be20b184
---

Scale-to-zero is usually a cloud or container thing. Google Cloud Run, AWS Lambda, Fly.io Machines — they all do it by pausing or destroying containers. If you're running apps on your own servers with native processes, you're expected to keep them running 24/7.

Tako does it differently. Your app scales to zero and cold-starts on demand, with no containers involved.

## How it works

Every Tako app starts with desired instances set to `0` — on-demand mode. Here's the lifecycle:

```d2
direction: down

deploy: Deploy {style.fill: "#9BC4B6"; style.font-size: 20}
warm: Warm instance {style.fill: "#9BC4B6"; style.font-size: 20}
serving: Serving {style.fill: "#9BC4B6"; style.font-size: 20}
idle: Idle timeout {style.fill: "#E88783"; style.font-size: 20}
zero: Zero instances {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 20}
cold: Cold start {style.fill: "#E88783"; style.font-size: 20}
back: Serving again {style.fill: "#9BC4B6"; style.font-size: 20}

deploy -> warm: start 1 instance
warm -> serving: request arrives
serving -> idle: no requests for 5 min
idle -> zero: instance stopped
zero -> cold: next request arrives
cold -> back: "often 10s of ms"
```

**Deploy.** When you run [`tako deploy`](/docs/deployment/), the server starts one warm instance immediately — so your app is reachable right away. If that instance fails to start, the deploy fails. No surprise cold starts after shipping.

**Serve.** Requests route to healthy instances through Tako's [Pingora-based proxy](/blog/pingora-vs-caddy-vs-traefik/). Each instance tracks in-flight requests and the timestamp of its last request.

**Idle.** An idle monitor checks instances periodically. If an instance has no in-flight requests and has been idle longer than `idle_timeout` (default: 5 minutes), it gets stopped. The app drops to zero running instances.

**Cold start.** The next request triggers a cold start. The proxy spawns a new process, waits for the app's readiness signal (`TAKO:READY:<port>` via the [SDK](/docs/)), and routes the request once the instance is healthy. For lightweight APIs, that first response is often only tens of milliseconds slower. Heavier apps can take longer.

## What happens to requests during cold start

This is the tricky part. What if 50 requests arrive while the app is booting?

Tako uses a leader/waiter pattern. The first request becomes the "leader" and triggers the instance spawn. Every subsequent request becomes a "waiter" and queues behind it. Up to 1000 requests can queue per app. When the instance is ready, all waiters are unblocked simultaneously.

| Scenario                    | Response                                                |
| --------------------------- | ------------------------------------------------------- |
| Instance starts in time     | Normal response (after cold start delay)                |
| Startup exceeds 30s         | `504 App startup timed out`                             |
| Process crashes on start    | `502 App failed to start`                               |
| Queue exceeds 1000 requests | `503 App startup queue is full` (with `Retry-After: 1`) |

Instances are never killed while serving in-flight requests. The idle monitor only stops instances that are both idle _and_ have zero active connections.

## Why this matters for cost

If you're running one app per server, scale-to-zero doesn't save much. But most people don't run one app per server.

A typical Tako setup might have a production API (always-on), plus a staging environment, an admin dashboard, a webhook processor, and a docs site — all on the same box. Without scale-to-zero, each of those keeps processes running around the clock. A Node.js process idles at 50-100MB. Five idle apps? That's 250-500MB of RAM doing nothing.

With Tako's on-demand model, those low-traffic apps consume zero resources when idle. The staging environment that nobody touches on weekends? Gone. The admin panel your team uses twice a day? Boots in 200ms when someone opens it.

This is especially useful on VPS instances where RAM is the constraint. A $6/month box with 1GB of RAM can comfortably host a handful of apps when most of them aren't loaded into memory at the same time.

## Configuration

Scale-to-zero is the default. You don't need to configure anything for it to work. But you can tune it:

```toml
# tako.toml
[envs.production]
idle_timeout = 300  # seconds (default: 5 minutes)

[envs.staging]
idle_timeout = 60   # aggressive timeout for staging
```

For always-on apps, use [`tako scale`](/docs/cli/) to set a minimum instance count:

```bash
tako scale 2 --env production  # always keep 2 instances running
```

This persists across deploys, rollbacks, and server restarts.

## Not serverless

This isn't serverless. There's no per-request billing, no function isolation, no event-driven invocation model. Your app is a normal long-running process — it just doesn't run when nobody's using it.

The cold start is a real process spawn, not a container unpause or a microVM boot. That's why it's fast: no image layers to unpack, no filesystem to mount, no network namespace to create. Just fork, exec, wait for readiness.

And because Tako's proxy handles the queuing transparently, your app doesn't need to know it was cold-started. No special warming logic, no readiness hacks. The [SDK's status endpoint](/docs/) is enough.

## Try it

Every Tako app gets scale-to-zero out of the box. Deploy anything and watch it idle down after 5 minutes of quiet:

```bash
tako deploy
tako status  # see instance count drop to 0
# visit your app — it cold-starts on the first request
```

Check the [deployment docs](/docs/deployment/) for the full setup, or [how Tako works](/docs/how-tako-works/) for the architecture behind on-demand scaling.
