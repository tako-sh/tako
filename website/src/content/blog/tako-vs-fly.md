---
title: "Tako vs Fly.io: The Self-Hosted Edge"
date: "2026-04-11T15:33"
description: "Fly.io gives you a beautiful CLI and 30+ regions on their hardware. Tako gives you the same feel on your own boxes. An honest comparison of two takes on the edge."
image: 869741dfb914
---

[Fly.io](https://fly.io) is one of our favorite developer platforms. A CLI that feels great, micro-VMs that boot fast enough to scale to zero, and 30+ regions you can scatter across with a single command. If you want "global" without thinking about servers, it's hard to beat.

Tako plays the same sport differently. Same goal — your app, running close to users, deployed with one command — but the hardware is yours, the proxy is yours, and the bill is one you already know.

## At a glance

|                   | **Fly.io**                          | **Tako**                                               |
| ----------------- | ----------------------------------- | ------------------------------------------------------ |
| **Model**         | Hosted PaaS                         | Self-hosted platform                                   |
| **Runtime unit**  | Firecracker micro-VM from OCI image | Native OS process                                      |
| **Deploy input**  | Dockerfile / container image        | Built artifact over SFTP                               |
| **Proxy**         | fly-proxy (anycast)                 | [Pingora](/blog/pingora-vs-caddy-vs-traefik) (Rust)    |
| **Regions**       | 30+ built-in                        | Whichever VPS you rent, wherever you rent it           |
| **Scale-to-zero** | Yes (autostop / autostart)          | Yes ([idle instances spin down](/docs/how-tako-works)) |
| **Pricing**       | Per-VM, per-second, per-GB egress   | Whatever your VPS already costs                        |
| **Lock-in**       | Fly platform                        | None — it's your box                                   |
| **Local dev**     | Docker / separate tooling           | Built-in ([`tako dev`](/docs/development))             |
| **CLI**           | `flyctl`                            | `tako`                                                 |

## Where Fly.io shines

Fly.io nailed something we deeply respect: the CLI-first cloud. `fly launch` on an empty folder and a minute or two later your app is serving HTTPS, close to your users. No dashboards, no consoles. Everything lives in `fly.toml` and `flyctl`.

Their runtime is genuinely cool. They take your OCI image, convert it, and boot it inside a Firecracker micro-VM — fast enough that they can stop idle machines and start them back up on the next request. Combined with anycast routing from their proxy, every user hits the closest healthy instance without you lifting a finger.

And the breadth matters. Managed Postgres, private networking between machines, GPUs for ML workloads, volumes, LiteFS — if you need it, Fly probably has it. For a lot of teams, that's the right tradeoff: pay for convenience, ship faster, think about servers less.

## Where Tako is different

### It's your box

The biggest difference isn't technical, it's structural. Fly runs on Fly's hardware. Tako runs on whatever Linux box you already pay for — a $5 Vultr, a Hetzner CX22, a Linux machine in your closet. When you deploy, there's no platform bill on top, no per-gigabyte egress, no autoscaling surprise at the end of the month. Your costs are whatever your VPS bill already is.

### No Docker in the path

Fly's deploy input is an OCI image. You write a Dockerfile, they convert it into a micro-VM. It works, but it puts a whole layer of tooling between your source and the process that actually serves requests.

Tako has [no Docker requirement](/blog/why-we-dont-default-to-docker). You build locally with whatever toolchain you'd use anyway, and [`tako deploy`](/docs/deployment) sends the artifact straight to the server over SFTP. The thing running on the box is literally `bun run` (or `node`, or your Go binary) — a native process managed by `tako-server`. Fewer layers, fewer things to debug when something goes sideways.

### Your own map

Fly gives you 30+ regions for free. Tako gives you however many regions you feel like paying for — and you choose the providers. Drop one line in [`tako.toml`](/docs/tako-toml):

```toml
[envs.production]
route = "myapp.com"
servers = ["la", "fra", "sgp"]
```

Register each box once with `tako servers add`, and the next [`tako deploy`](/docs/cli) ships to all three in parallel. Point Cloudflare geo-steering at them and users hit the closest one.

It's a little more work up front, but you control the map. Want a region Fly doesn't offer? Rent a box there. Want a cheap regional provider in São Paulo or Warsaw? Nothing is stopping you. We walked through the whole setup in [Build Your Own Edge Network on Commodity Hardware](/blog/build-your-own-edge-network-on-commodity-hardware).

### A platform that keeps growing

Fly is a rock-solid runtime for your code. Tako wants to be more than a runtime. Deployment, routing, TLS, [secrets](/blog/secrets-without-env-files), [local dev with real HTTPS](/blog/local-dev-with-real-https), and [zero-downtime rolling deploys](/blog/zero-downtime-deploys-without-a-container-in-sight) ship today. WebSocket channels, queues, workflows, and image optimization are on the roadmap — backend primitives that otherwise live in a separate service, built into the same server binary that's already running your app.

## When each makes sense

Pick **Fly.io** if you want a hosted platform that handles hardware for you, if your app leans on their managed services (Postgres, GPUs, private networking), or if "zero servers to think about" is worth a variable monthly bill. They earn it.

Pick **Tako** if you already rent VPS boxes — or want to — and you'd rather own the whole stack end to end. Same CLI-first feel, same scale-to-zero, same kind of edge distribution. Different hardware, different bill, different amount of control.

Both answers are reasonable. We're building Tako for the people who've read this far and already know which side they're on.

[Start with the docs →](/docs)
