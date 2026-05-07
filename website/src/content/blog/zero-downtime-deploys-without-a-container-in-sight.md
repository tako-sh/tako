---
title: "Zero-Downtime Deploys Without a Container in Sight"
date: "2026-04-07T04:43"
description: "How Tako rolls out new versions with connection draining, health-checked readiness, and automatic rollback — all with native processes."
image: 170a5555e1fb
---

Zero-downtime deploys usually mean containers. Kubernetes rolling updates, Docker Swarm service convergence, Fly.io machine replacement. The assumption is that you need an abstraction layer — something that can spin up a fresh container, health-check it, and tear down the old one.

Tako does all of that with native processes. No Docker, no container runtime, no image registry. Just your app, a [Pingora-based proxy](/blog/pingora-vs-caddy-vs-traefik), and a unix socket protocol that orchestrates the whole thing.

## The rolling update sequence

When you run [`tako deploy`](/docs/deployment), the CLI builds your app locally, uploads the artifact via SFTP, and sends a `Deploy` command over the server's unix socket. What happens next is a one-at-a-time rolling update:

```d2
direction: down

start: Deploy command received {style.fill: "#9BC4B6"; style.font-size: 18}
spawn: Spawn new instance {style.fill: "#9BC4B6"; style.font-size: 18}
ready: Wait for TAKO:READY {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 18}
health: Health check passes {style.fill: "#9BC4B6"; style.font-size: 18}
drain: Drain old instance {style.fill: "#E88783"; style.font-size: 18}
wait: Wait for in-flight requests {style.fill: "#E88783"; style.font-size: 18}
stop: Stop old process {style.fill: "#E88783"; style.font-size: 18}
done: Repeat for next instance {style.fill: "#9BC4B6"; style.font-size: 18}

start -> spawn: batch size: 1
spawn -> ready: "stdout: TAKO:READY:12345"
ready -> health: probe /status
health -> drain: mark old as Draining
drain -> wait: "max 30s"
wait -> stop: kill process
stop -> done
```

The key detail: the old instance isn't touched until the new one is verified healthy. If the new instance fails to start or its health check times out (30 seconds), Tako kills the new instance and keeps the old ones running. Automatic rollback, no intervention needed.

## How readiness actually works

Most deploy tools check health by poking a TCP port. If the socket accepts connections, the app must be ready. But that's a guess — your server might be listening while still loading config or running migrations.

Tako uses an explicit readiness signal. The [SDK](/docs) handles this automatically:

1. Your app starts, runs any initialization (DB connections, cache warming)
2. The SDK binds to an OS-assigned port (`PORT=0`)
3. It writes `TAKO:READY:12345` to stdout
4. The server picks up the port and begins health probing

Your app can also define a `ready()` hook for custom initialization logic — the SDK won't signal readiness until it completes. This means traffic only reaches instances that are genuinely ready to serve.

Once ready, the server probes every 1 second with a request to the SDK's built-in `/status` endpoint (using the internal `Host: tako` header). One failed probe marks the instance unhealthy and pulls it from the load balancer.

## Connection draining

This is where zero-downtime actually happens. When an old instance enters the `Draining` state:

1. The [load balancer](/docs/how-tako-works) stops sending it new requests
2. In-flight requests continue to completion (up to 30 seconds)
3. Once the in-flight counter hits zero, the process is killed

No request is ever dropped mid-response. The proxy tracks active connections per instance, and draining is a hard guarantee — not a best-effort grace period.

| Phase             | Timeout     | What happens on timeout                        |
| ----------------- | ----------- | ---------------------------------------------- |
| Startup readiness | 30s         | New instance killed, old instances kept        |
| Health check      | 1s interval | 1 failure → unhealthy, removed from rotation   |
| Connection drain  | 30s         | Process killed (in-flight requests terminated) |

## The protocol under the hood

The CLI and server communicate over a unix socket at `/var/run/tako/tako.sock` using newline-delimited JSON. A deploy sends two commands:

**`PrepareRelease`** — extracts the artifact, downloads the runtime (Bun or Node), and runs `npm ci` / `bun install`. This happens _before_ any instance swap, so dependency installation doesn't eat into your downtime window.

**`Deploy`** — carries the app name, version, release path, routes, and (optionally) secrets. This triggers the rolling update. Secrets are delivered to each new instance via file descriptor 3 — they never touch disk or environment variables. If the secrets hash hasn't changed since the last deploy, they're [skipped entirely](/blog/secrets-without-env-files).

The two-phase design keeps instance startup fast. By the time `Deploy` fires, everything is already installed. The new process just needs to boot your app and signal readiness.

## No containers required

The entire flow — spawn, health-check, drain, kill — is the same pattern that Kubernetes uses for rolling deployments. The difference is that Tako does it with native processes managed by a single Rust binary, proxied through Pingora.

No Docker daemon. No image layers. No container networking. Just processes, a proxy, and a protocol.

Check out the [deployment guide](/docs/deployment) for the full setup, or [how Tako works](/docs/how-tako-works) for the architecture behind it.
