---
title: "Build Your Own Edge Network on Commodity Hardware"
date: "2026-04-07T04:51"
description: "Three $5 VPS boxes in different regions, one tako.toml, and Cloudflare geo-steering. Your own global edge network on hardware you own."
image: 755f99939072
---

Fly.io lets you scatter micro-VMs across 30+ regions with a single command. It's genuinely great. It's also someone else's hardware with someone else's pricing — and those micro-VMs come with [256 MB of RAM](/blog/your-5-dollar-vps-is-more-powerful-than-you-think).

What if you could get the same geographic distribution on servers with 16x the memory, for a similar monthly bill? Three VPS boxes, one [`tako.toml`](/docs/tako-toml), and Cloudflare routing users to the nearest one.

## The architecture

```d2
direction: right

users: Users {
  us: US West {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
  eu: Europe {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
  asia: Asia Pacific {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
}

cf: Cloudflare\ngeo-steering {
  shape: cloud
  style.fill: "#9BC4B6"
  style.font-size: 18
}

la: LA\nVultr {shape: hexagon; style.fill: "#E88783"; style.font-size: 16}
fra: Frankfurt\nHetzner {shape: hexagon; style.fill: "#E88783"; style.font-size: 16}
sgp: Singapore\nVultr {shape: hexagon; style.fill: "#E88783"; style.font-size: 16}

users.us -> cf
users.eu -> cf
users.asia -> cf

cf -> la: nearest
cf -> fra: nearest
cf -> sgp: nearest
```

Three layers. **Cloudflare** sits at the edge — handles DNS, terminates TLS, and routes each request to the nearest origin. **Tako** runs on each VPS — [deploys your app](/docs/deployment), manages processes, handles zero-downtime rolling updates. **Your servers** do the actual compute.

A user in Tokyo hits Singapore. Berlin goes to Frankfurt. San Francisco goes to LA. Each server runs the same app, deployed from the same config, completely independent of the others.

## The config

We covered multi-server config in detail in [One Config, Many Servers](/blog/one-config-many-servers). The short version:

```toml
name = "myapp"

[build]
run = "bun run build"

[envs.production]
route = "myapp.com"
servers = ["la", "fra", "sgp"]
```

Register each server once with [`tako servers add`](/docs/cli), then `tako deploy` builds your app once locally and uploads it to all three servers in parallel via SFTP. Each server runs its own [rolling update](/blog/zero-downtime-deploys-without-a-container-in-sight) independently — if Frankfurt finishes before Singapore, it starts serving the new version immediately.

## The routing layer

You have two options, depending on how much you want to spend on intelligence.

**Free: round-robin DNS.** Add three proxied A records for `myapp.com` in Cloudflare. Requests distribute across your origins, and Cloudflare retries failed ones automatically. You get failover for free, but no geo-awareness — a user in Tokyo might hit LA.

**Smart: geo-steering ($20/mo).** [Cloudflare Load Balancing](https://developers.cloudflare.com/load-balancing/) with geo-steering assigns each origin to a geographic region. Health checks run every 60 seconds — if Singapore goes down, traffic fails over to the next nearest server automatically.

| Component                       | Cost       |
| ------------------------------- | ---------- |
| Base (2 origins, health checks) | $5/mo      |
| 3rd origin                      | $5/mo      |
| Geo-steering add-on             | $10/mo     |
| **Total**                       | **$20/mo** |

The free option is a fine starting point. Upgrade to geo-steering when latency starts to matter.

## The cost

| Setup                             | Monthly  | RAM per region |
| --------------------------------- | -------- | -------------- |
| 3x Hetzner CX22 + CF round-robin  | **~$18** | 4 GB           |
| 3x Vultr $5 + CF round-robin      | **$15**  | 1 GB           |
| 3x Hetzner CX22 + CF geo-steering | **~$38** | 4 GB           |
| Fly.io shared-cpu-1x, 256 MB × 3  | **~$8**  | 256 MB         |
| Fly.io shared-cpu-1x, 1 GB × 3    | **~$20** | 1 GB           |

Fly.io wins on simplicity and minimum price. But look at the resources: three Hetzner boxes give you **4 GB per region** — 16x Fly.io's cheapest tier. For $38/month with geo-steering, you get 12 GB of total RAM across three continents on machines you fully control. Without geo-steering, $18/month still gives you three globally distributed servers with automatic failover.

And those VPS boxes can each host [multiple apps](/blog/scale-to-zero-without-containers) that scale to zero when idle — your edge network doesn't have to serve just one project.

## No control plane

Each Tako server is self-sufficient. Its own [Pingora proxy](/blog/pingora-vs-caddy-vs-traefik), its own process management, its own [secrets](/blog/secrets-without-env-files) database, its own release history. There's no cluster state, no leader election, no orchestrator to babysit. If one server disappears, the others keep serving and Cloudflare stops routing to it.

Adding a region later is a three-step process: spin up a VPS, run `tako servers add`, add the name to your `servers` list. Next deploy, it's live. Removing one is the reverse — take it out of the list, decommission the box.

## The bigger picture

This is the floor. The same config that deploys to three servers also handles [per-server scaling](/blog/one-config-many-servers), [per-environment secrets](/docs/cli), and a [staging environment](/blog/one-config-many-servers) that costs nothing when nobody's using it.

And Tako is growing past deploys. The [SDK](/docs/how-tako-works) is the starting point for backend primitives — WebSocket channels, queues, workflows — running on the same servers, managed from the same config. Three boxes in three regions, each with the full platform layer, all from one `tako.toml`.

Your edge network doesn't need to be someone else's infrastructure. A few cheap VPS boxes and the right tools get you surprisingly far.

[Get started →](/docs)
