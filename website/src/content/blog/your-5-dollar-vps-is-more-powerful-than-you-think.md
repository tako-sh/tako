---
title: "Your $5 VPS Is More Powerful Than You Think"
date: "2026-04-07T04:38"
description: "A $5 Hetzner box gives you 2 vCPUs, 4 GB RAM, and 20 TB bandwidth. Here's what cloud platforms charge for the same resources — and how Tako bridges the gap."
image: baa7089d6301
---

Look at what a $5 Hetzner box actually gives you: 2 vCPUs, 4 GB of RAM, 40 GB of NVMe storage, and 20 TB of monthly bandwidth. That's a real machine. Now look at what cloud platforms charge for comparable resources.

## The numbers

Here's what ~$5-7/month gets you from a VPS provider:

| Provider         | Price  | vCPUs | RAM  | Storage    | Bandwidth |
| ---------------- | ------ | ----- | ---- | ---------- | --------- |
| Hetzner CX22     | ~$6/mo | 2     | 4 GB | 40 GB NVMe | 20 TB     |
| OVHcloud Starter | ~$4/mo | 1     | 2 GB | 20 GB SSD  | Unmetered |
| Vultr            | $5/mo  | 1     | 1 GB | 25 GB SSD  | 1 TB      |
| DigitalOcean     | $6/mo  | 1     | 1 GB | 25 GB SSD  | 1 TB      |

And here's what equivalent money gets you on a cloud platform:

| Platform        | Price     | vCPUs      | RAM     | Notes                            |
| --------------- | --------- | ---------- | ------- | -------------------------------- |
| Render Starter  | $7/mo     | 0.5 shared | 512 MB  | Minimum always-on tier           |
| Fly.io          | ~$8-10/mo | 1 shared   | 1 GB    | Includes IPv4 ($2) + volume      |
| Railway Hobby   | $5/mo     | ~0.25      | ~512 MB | $5 usage credit; overages common |
| Render Standard | $25/mo    | 1          | 2 GB    | First tier with usable resources |

Hetzner's $6 box has **8x the RAM** of Render's $7 tier. Fly.io charges $2/month just for an IPv4 address — something every VPS includes for free. Railway's $5 plan includes $5 of compute credit, which covers roughly a quarter-vCPU running continuously. You burn through it fast.

The VPS isn't slightly cheaper. It's a different category of value.

## So why does anyone pay more?

Because compute was never the hard part. The hard part is everything around it:

- **TLS certificates** — setting up Let's Encrypt, auto-renewal, wildcard domains
- **Reverse proxy** — routing traffic, handling multiple apps on one box
- **Deployments** — zero-downtime deploys, rollbacks, not SSH-and-pray
- **Secrets** — not committing `.env` files, encrypted storage, per-environment values
- **Local dev** — matching production behavior without a Docker Compose novel

Cloud platforms bundle all of this into their pricing. You're not paying $25/month for 1 vCPU and 2 GB of RAM. You're paying for the fact that deploys, routing, and TLS just work.

That's a real tradeoff. But it doesn't have to be.

## What Tako handles

[Tako](/docs/) runs on your VPS and handles the platform layer — the stuff between your code and the internet. One binary, no Docker, no Kubernetes.

```bash
tako deploy production
```

That command builds your app locally, uploads the artifact over SFTP, and performs a [zero-downtime rolling update](/docs/deployment/) with health checks. If something goes wrong, `tako releases rollback` puts you back instantly.

Here's what you get out of the box:

- **Automatic TLS** — [Let's Encrypt certificates](/docs/how-tako-works/) issued and renewed for every route, including [wildcards](/docs/tako-toml/)
- **Routing** — Exact domains, wildcard subdomains, path-based routes, static files — all via [Pingora](/blog/pingora-vs-caddy-vs-traefik/), Cloudflare's proxy framework
- **Secrets** — [Encrypted at rest](/blog/secrets-without-env-files/), per-environment, injected via file descriptor (never touch disk on the server)
- **Local dev** — [`tako dev`](/blog/local-dev-with-real-https/) gives you real HTTPS at `https://myapp.test` with zero config
- **Scale to zero** — [Apps sleep when idle](/blog/scale-to-zero-without-containers/) and wake on the next request, so one VPS can host many projects
- **Multi-server** — Deploy across [multiple servers and environments](/blog/one-config-many-servers/) from a single `tako.toml`

Your `tako.toml` stays minimal:

```toml
[app]
name = "myapp"
preset = "nextjs"

[envs.production]
servers = ["hetzner-1"]
routes = ["myapp.com"]
```

## The math

Say you're running three small projects — a marketing site, an API, and a side project. On Render, that's $21-75/month minimum. On Railway, you'd likely exceed the Hobby credit on project two.

On a single Hetzner CX22 with Tako, that's ~$6/month total. All three apps get their own domains, TLS, and zero-downtime deploys. When traffic is low, idle apps [scale to zero](/blog/scale-to-zero-without-containers/) and free up resources for the ones that need them.

When you outgrow one box, add another server to your environment and [deploy across both](/blog/one-config-many-servers/). Still cheaper than one Render Standard instance.

## Your hardware, your rules

Cloud platforms are great products. If you want managed databases, built-in CI, and a team dashboard out of the box, they deliver real value.

But if you're an indie dev watching costs, a VPS gives you dramatically more compute per dollar. The only thing missing is the platform layer — and that's exactly [what Tako is](https://github.com/tako-sh/tako).

Your $5 VPS was always powerful enough. It just needed the right tools.
