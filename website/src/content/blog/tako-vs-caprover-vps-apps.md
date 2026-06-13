---
title: "Tako vs CapRover for VPS Apps"
date: "2026-06-13T08:15"
description: "Compare CapRover's open-source dashboard PaaS with Tako's CLI-first VPS platform: deploys, HTTPS, secrets, workflows, and containers."
image: 14ec9e1ce95b
---

[CapRover](https://caprover.com/) is one of the friendliest open-source ways to turn a VPS into a self-hosted PaaS. Install it, point a wildcard domain at the box, open the dashboard, and you get app deploys, domains, HTTPS, logs, environment variables, Docker images, and one-click apps in a browser.

That is a good product shape. It gives people a home base.

Tako starts from a different bet: the center of gravity should be your repo, your terminal, and your app runtime. The platform should still handle the annoying parts — deploys, routing, TLS, secrets, logs, scale-to-zero, workflows, and now [opt-in containers](/blog/how-to-deploy-a-dockerfile-to-a-vps-with-tako-container-releases/) — but the primary interface should be a small CLI and a versioned [`tako.toml`](/docs/tako-toml/).

Same VPS instinct. Different control surface.

## At A Glance

| Concern            | **CapRover**                                                                     | **Tako**                                                                                         |
| ------------------ | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| Primary interface  | Web dashboard, plus CLI                                                          | CLI-first, config in `tako.toml`                                                                 |
| Runtime model      | Docker containers on Docker Swarm                                                | Native Bun, Node, and Go processes by default; Podman containers when `container = "Dockerfile"` |
| Deploy input       | `captain-definition`, tar upload, Dockerfile/image, CI action, or dashboard flow | `tako deploy` builds locally, uploads over SFTP, or packages source for a container release      |
| Proxy/TLS          | nginx plus Let's Encrypt integration                                             | Pingora proxy with Let's Encrypt or Cloudflare SSL modes                                         |
| Secrets/config     | App environment variables in CapRover app config                                 | Encrypted project secrets, injected through SDK bootstrap data                                   |
| Local dev          | Bring your own local workflow                                                    | Built-in [`tako dev`](/docs/development/) with HTTPS, local DNS, logs, and SDK runtime           |
| Scaling            | App instance count on the server/cluster                                         | Persistent desired instance count, including scale-to-zero                                       |
| Background work    | Run another app/container or external worker stack                               | Durable workflows built into the app platform                                                    |
| One-click services | Strong fit: many one-click apps and Docker Compose-shaped templates              | Not the goal; pair Tako with the services you choose                                             |
| Container support  | Core model                                                                       | Opt-in for Dockerfile-shaped apps                                                                |

CapRover is closer to a mini Heroku dashboard for your own box. Tako is closer to an app platform layer that happens to live on your own box.

## CapRover's Sweet Spot

CapRover shines when you want a visual control panel for a server. You can create an app, attach a domain, enable HTTPS, set environment variables, inspect logs, scale instance count, and deploy Docker-based workloads without building your own nginx or Docker Swarm wiring.

It also has a broad app surface. The [CapRover one-click app model](https://caprover.com/docs/one-click-apps.html) can deploy images and Compose-shaped templates, which makes it useful for WordPress, databases, dashboards, and packaged open-source services. If your goal is "I want my VPS to have a place where I can install and manage lots of server software," CapRover has the right mental model.

The deployment model is also flexible. CapRover documents multiple paths: CLI deploys, dashboard uploads, CI/CD flows, Docker images, and `captain-definition` files. A heterogeneous team can use a browser for day-to-day management and still automate deploys later.

That dashboard-first model is the point. It gives you an admin surface for the whole machine.

## Where Tako Is Different

Tako keeps the machine mostly out of sight. A project carries its deployment shape in config:

```toml
name = "api"
runtime = "bun"
main = "src/index.ts"

[envs.production]
route = "api.example.com"
servers = ["prod"]
idle_timeout = 300
```

Then the workflow is boring:

```bash
tako secrets set DATABASE_URL --env production
tako deploy
tako logs --env production --tail
```

The server is still doing real platform work. It runs the Pingora proxy, terminates TLS, tracks app instances, performs health checks, stores secrets encrypted server-side, streams logs, and rolls releases forward only after fresh instances are healthy. But the source of truth is the repo plus the CLI, not a browser dashboard database.

The native deploy path is the other big difference. CapRover is Docker-first. Tako defaults to native processes for the runtimes it knows deeply: Bun, Node, and Go. A deploy builds locally, uploads a compressed artifact over SFTP, runs production install when needed, and starts the app directly under `tako-server`. The [deployment docs](/docs/deployment/) spell out the full release flow.

That matters for small VPS apps because most of them do not need a container boundary. They need a good proxy, HTTPS, secrets, logs, health checks, and a reliable rolling update.

```d2
direction: right

repo: "Repo\ncode + tako.toml" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
cli: "tako deploy\nlocal build" {style.fill: "#9BC4B6"; style.font-size: 16}
server: "VPS\ntako-server" {style.fill: "#E88783"; style.font-size: 16}
runtime: "App runtime\nnative process or container" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
platform: "Platform layer\nTLS, routing, logs, secrets, workflows" {style.fill: "#9BC4B6"; style.font-size: 16}
users: "Users\nhttps://app.example.com" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}

repo -> cli
cli -> server: "SFTP artifact"
server -> runtime: "start + probe"
runtime -> platform: "SDK contract"
platform -> users
```

Tako now has an answer for Dockerfile-shaped apps too. Set:

```toml
runtime = "go"
container = "Dockerfile"
dev = ["go", "run", "."]
```

With `container = "Dockerfile"`, production deploys package source for a container release. The server builds the image with Podman, starts HTTP containers from the image defaults, and still keeps Tako routing, TLS, secrets, logs, and rolling updates around it. Containers are not the default abstraction, but they are available when they are the honest fit.

## Platform Primitives, Not Just App Starts

CapRover is a strong app and service manager. Tako is deliberately narrower on server administration and deeper on app runtime primitives.

Scale-to-zero is a good example. In Tako, a deployed app has a persistent desired instance count. New apps start warm, but you can opt into on-demand serving:

```bash
tako scale 0 --env production
```

When desired instances are `0`, the server can stop idle HTTP instances after the app's idle timeout and cold-start them on the next request. That is useful for preview apps, staging routes, admin tools, webhook handlers, and the dozen tiny services that do not need to burn memory all day.

Workflows are another example. CapRover can run worker containers, and that is a perfectly normal Docker answer. Tako adds durable workflows to the app model itself. JavaScript apps define workflow files under `<app_root>/workflows/`; the server owns the queue, retries, step checkpoints, schedules, and worker lifecycle. Workers can scale to zero separately from HTTP traffic. The [how Tako works](/docs/how-tako-works/) guide covers the runtime protocol behind that.

Secrets also show the difference. CapRover exposes environment-variable configuration for apps. Tako stores project secrets encrypted in `.tako/secrets.json`, validates expiry during deploy, sends secret updates over signed management paths, stores them encrypted on the server, and injects them into fresh app processes through the SDK bootstrap envelope. Native processes receive fd 3; container releases receive `TAKO_BOOTSTRAP_DATA`. App code reads `tako.secrets` or generated Go helpers, not plaintext files.

The pattern is consistent: CapRover gives the operator a dashboard for containers and services. Tako gives the app a runtime contract.

## Choose The Shape You Want

Choose CapRover if you want a dashboard PaaS for a VPS, you like Docker as the universal app boundary, you want one-click apps and packaged services, or your team prefers browser-based operations. It is friendly, practical, and broad.

Choose Tako if your deploy workflow should live in git, your team prefers a CLI, you want native deploys for Bun, Node, or Go, you care about [local dev with real HTTPS](/docs/development/), or you want app primitives like scale-to-zero and durable workflows without assembling another stack.

And if the thing you need is a Dockerfile, Tako does not make you leave. Use a container release, keep the Dockerfile, and let Tako keep handling the platform layer around it.

That is the difference we are building around: not a bigger dashboard than CapRover, but a smaller control surface around a deeper app runtime.
