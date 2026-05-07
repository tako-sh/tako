---
title: "Tako vs Dokku: Two Philosophies for Self-Hosted Deployment"
date: "2026-04-04T14:32"
description: "Dokku wraps everything in Docker. Tako runs your app directly. Here's how the two approaches compare — and when each one makes sense."
image: 6e6d653dcf57
---

[Dokku](https://dokku.com) has been around since 2013. It's the original "mini-Heroku" — a `git push` and your app is live in a Docker container, complete with Heroku buildpacks, nginx routing, and a plugin ecosystem that covers databases, caching, and more. With ~32k GitHub stars and over a decade of production use, it's earned its reputation.

Tako takes a different path. We think there's a faster, simpler way to deploy web apps to your own servers — one that doesn't require Docker at all.

## The Core Difference

Dokku puts Docker at the center of everything. Every app runs in a container. Builds go through buildpacks (Heroku-style, Cloud Native Buildpacks, Nixpacks, or Dockerfiles). The deploy flow is: `git push` → build image → run container → route traffic via nginx.

Tako skips the container layer entirely. Your app runs as a native process under its own runtime — Bun, Node.js, or Go. The deploy flow is: build locally → [SFTP the artifact](/docs/deployment) → start the process → route traffic via [Pingora](/blog/pingora-vs-caddy-vs-traefik).

|                   | Dokku                            | Tako                                                |
| ----------------- | -------------------------------- | --------------------------------------------------- |
| **Deploy method** | `git push` → build on server     | `tako deploy` → build locally, SFTP                 |
| **App isolation** | Docker containers                | Native OS processes                                 |
| **Proxy**         | nginx (default), Traefik, Caddy  | Pingora (Cloudflare's Rust framework)               |
| **Builders**      | Buildpacks, Dockerfile, Nixpacks | Direct runtime execution                            |
| **Language**      | Shell + Go                       | Rust                                                |
| **Multi-server**  | Single server                    | Multi-server via [environments](/docs/deployment)   |
| **Local dev**     | Separate tooling                 | Built-in [`tako dev`](/docs/development) with HTTPS |
| **Stars**         | ~32k                             | Newer project                                       |

## Where Dokku Shines

Dokku's Docker-first model means it can run _anything_ — Python, Ruby, Java, Elixir, Rust, or a hand-rolled Dockerfile. If your stack is heterogeneous or you need specific system libraries baked into a container image, Dokku handles that naturally.

The plugin ecosystem is mature. Need Postgres? `dokku postgres:create mydb`. Redis? Same pattern. Let's Encrypt? One plugin. After 13 years, most common needs have a plugin.

And `git push` deploys are genuinely elegant. Push to a remote, Dokku takes it from there. No local build step, no artifact management.

## Where Tako Is Different

**Speed.** Building locally and sending a compressed artifact via SFTP is faster than building Docker images on your server. No registry round-trips, no layer rebuilds, no image pulls. A typical deploy takes seconds, not minutes.

**No Docker required.** Your server doesn't need the Docker daemon running. No images to manage, no container runtime overhead, no debugging Dockerfile layer caching. Your app is just a process.

**Integrated local development.** [`tako dev`](/docs/development) runs your app with the same [SDK](/blog/why-tako-ships-an-sdk) and runtime as production, with local HTTPS and DNS routing out of the box. Dokku is a server-side tool — local development is up to you.

**Multi-server deployments.** Define [environments](/docs/deployment) in your [`tako.toml`](/docs/tako-toml) — production on multiple servers, staging on another — and deploy to all of them with one command. Dokku is designed for a single server. This matters more than it sounds: spin up cheap VPS boxes in LA, NYC, Frankfurt, and Singapore, deploy to all of them at once, put Cloudflare in front — and you've got your own edge network. No Kubernetes, no Fly.io bill — just your hardware, everywhere.

```toml
[envs.production]
servers = ["la", "nyc", "fra", "sgp"]
```

**Rust + Pingora proxy.** Tako's proxy layer is built on [Cloudflare's Pingora framework](/blog/pingora-vs-caddy-vs-traefik), giving us tight integration between routing, process management, health checking, and cold starts — all in one async runtime.

## Beyond Deployment

Here's the bigger picture: Dokku is a deployment tool. A great one — but its job ends once your container is running and nginx is routing to it. Need WebSockets? Set up your own solution. Need a job queue? Bolt on Redis and a worker. Need image optimization? Add another service.

Tako is heading somewhere different. We're building **everything between your code and the internet** — a platform layer that sits behind your app and handles the infrastructure you'd otherwise assemble yourself. Tako server already serves your static files and assets directly, and manages [secrets](/docs/deployment) encrypted at rest — no plugins needed. What's coming: managed WebSocket and SSE channels, background queues, workflows, image optimization, and more — all built into the same server that's already running your app.

Your app handles your business logic. Tako handles everything underneath it.

## Different Tools, Different Tradeoffs

**Choose Dokku if** you want a proven, battle-tested platform that runs any language in Docker containers, you value a rich plugin ecosystem for managed services, or you prefer `git push` deploys without a local build step.

**Choose Tako if** you want a platform that goes beyond deployment — fast deploys without Docker, built-in local dev, multi-server environments, and a server layer that's growing into the backend infrastructure your app needs.

Both tools share the same conviction: you shouldn't need Kubernetes or a cloud PaaS to run your app. A VPS and a good tool should be enough. We just think the tool should do more than get your code running.

```bash
# Dokku's way
git push dokku master

# Tako's way
tako deploy
```

Two commands, same starting point — but Tako keeps going from there. Check out the [docs](/docs) to see how it all fits together.
