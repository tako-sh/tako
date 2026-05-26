---
title: "Tako vs Kamal"
date: "2026-04-05T00:00"
description: "How Tako and Kamal approach self-hosted deployment differently — Docker vs native processes, registries vs SFTP, and what each gets right."
image: b111b39dbd57
---

We love what <a href="https://x.com/dhh" target="_blank" rel="noopener noreferrer">DHH</a> and 37signals have done for self-hosted deployment. Kamal made "deploy to your own servers" cool again — and at 14k GitHub stars, it's the most prominent CLI deploy tool out there. If you're running apps on your own hardware, you've probably looked at it. We certainly did.

Tako does the same job differently. Both tools get your code onto your servers via SSH with zero-downtime deploys. But the architecture underneath is almost entirely different, and the long-term ambitions diverge too.

## At a glance

|                        | **Kamal**                       | **Tako**                                                |
| ---------------------- | ------------------------------- | ------------------------------------------------------- |
| **Deploy method**      | Docker build → registry → pull  | Build locally → SFTP upload                             |
| **Server requirement** | Docker engine                   | Just a Linux box                                        |
| **Proxy**              | kamal-proxy (Go)                | Pingora (Rust, Cloudflare)                              |
| **CLI language**       | Ruby                            | Rust                                                    |
| **Config format**      | YAML (`deploy.yml`)             | TOML ([`tako.toml`](/docs/tako-toml/))                  |
| **Local dev**          | None                            | Built-in HTTPS + DNS ([`tako dev`](/docs/development/)) |
| **SDK**                | None (health check endpoint)    | [JS/TS and Go SDKs](/docs/)                             |
| **Scale-to-zero**      | No                              | Yes, with cold start                                    |
| **Secrets**            | Adapter-based (1Password, etc.) | Encrypted locally, passed via fd 3                      |
| **Stars**              | ~14k                            | New kid on the block                                    |

## Where Kamal shines

Kamal deserves a lot of credit. Before Kamal, the self-hosted deploy space was mostly Dokku and a handful of smaller tools. DHH and 37signals made people rethink the cloud default — and that's something we deeply appreciate, because it's the same belief that drives Tako.

Kamal's biggest strength is Docker itself. If your team already runs Docker in production and has a container registry, Kamal slots right in. You write a Dockerfile, Kamal builds it, pushes it, and pulls it onto your servers. The container is the artifact — reproducible, isolated, portable.

The ecosystem is real. Kamal supports accessories (databases, Redis) as Docker containers alongside your app. It has adapters for 1Password, AWS Secrets Manager, and Bitwarden. The 37signals team runs their own production infrastructure on it — HEY, Basecamp, and more — which means it's genuinely battle-tested.

And kamal-proxy, their custom Go reverse proxy, handles the zero-downtime dance well: health check the new container, route traffic, drain the old one, clean up.

For Rails teams especially, it's a natural fit. Rails 7.1+ ships with a `/up` health check endpoint out of the box, and Kamal's conventions align perfectly with Rails conventions.

## Where Tako is different

### No Docker, no registry

Kamal's deploy pipeline has three network hops: build the Docker image, push it to a registry, then pull it onto each server. That registry — whether Docker Hub, GHCR, or ECR — is an external dependency that adds latency, cost, and a failure point.

Tako skips all of that. You build locally, and the artifact goes straight to the server over SFTP. No registry account, no image layers, no Docker daemon on the server. The only requirement is a Linux box with SSH access.

```d2
direction: right

kamal: Kamal {
  direction: down

  build: Docker build
  registry: Registry
  server: Server pull

  build -> registry: push
  registry -> server: pull
}

tako: Tako {
  direction: down

  build: Local build
  artifact: Build artifact
  server: Server

  build -> artifact: bundle
  artifact -> server: SFTP
}
```

### A real proxy

kamal-proxy is purpose-built for deployment coordination — health checks, connection draining, host routing. It does that job well, but it's not a production-grade reverse proxy. No HTTP/3, no advanced caching, no rate limiting. Teams that need those features put Nginx or Caddy in front of it.

Tako uses [Pingora](/blog/pingora-vs-caddy-vs-traefik/), Cloudflare's Rust proxy framework. It's the same technology that handles a significant chunk of internet traffic. TLS termination, HTTP/2, WebSocket proxying, and caching are built into the same process — no extra layer needed.

### Local development included

Kamal is a production deployment tool. Local dev is left to Docker Compose or whatever your team uses. There's no `kamal dev`.

Tako treats local dev as a first-class concern. [`tako dev`](/docs/development/) gives you real HTTPS with trusted certificates, local DNS routing (`*.test`), and a proxy that matches production behavior. Your app runs the same way locally as it does on the server — same SDK, same process model, same routing.

### Scale-to-zero

Kamal keeps your containers running. That's the right choice for always-on apps, but if you're running staging environments, internal tools, or low-traffic services, those containers still eat memory 24/7.

Tako supports [on-demand scaling](/docs/how-tako-works/): instances spin down after an idle timeout and cold-start on the next request. For apps that don't need to be always-on, this is meaningful resource savings on a single server running multiple apps.

### Lighter secrets

Kamal's secret management uses adapter files in `.kamal/secrets` that integrate with external vaults. Powerful if you're already using 1Password or AWS Secrets Manager, but it's another configuration surface to maintain and debug.

Tako encrypts secrets locally with AES-256-GCM and delivers them to app instances via file descriptor 3 at spawn time — never written to disk on the server, never in environment variables that might leak to logs. It's simpler to set up: [`tako secrets set`](/docs/cli/) and you're done.

## Different tools, different ambitions

Kamal is a great deploy tool, and we mean that sincerely. It's mature, well-maintained, and backed by a team that runs serious production infrastructure on it. If Docker is already part of your workflow, Kamal is an excellent choice.

Tako starts from a different place — no Docker, native processes, Pingora proxy — but the bigger difference is where it's headed. Kamal is a deployment tool and does that job well. Tako is becoming a platform: the layer between your code and the internet. Today that's deployment, routing, TLS, secrets, and local dev. Tomorrow it's WebSocket channels, queues, workflows, and more — backend primitives your app would otherwise need separate services for.

Combined with [multi-server environments](/docs/deployment/) and Cloudflare smart routing, Tako lets you build your own edge network on commodity VPS boxes. Think Fly.io, but on your own hardware.

The question isn't just "Docker or no Docker" — it's whether you want a deploy tool or a platform. We're grateful Kamal helped prove that self-hosting is a real option. We're building on that momentum.

Check out [how Tako works](/docs/how-tako-works/) for the full architecture, or the [deployment docs](/docs/deployment/) to see it in action.
