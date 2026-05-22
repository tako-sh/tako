---
title: "Tako vs PM2 + Nginx"
date: "2026-04-21T01:34"
description: "PM2 + Nginx is the quiet default for self-hosted Node — a stack of four tools glued together with shell scripts. Tako collapses it into one CLI."
image: fde56a5e241f
---

Before Docker, before Kubernetes, before the whole self-hosted PaaS genre showed up, there was a stack that shipped a lot of Node apps: [PM2](https://github.com/Unitech/pm2) keeping processes alive, Nginx out front for TLS and routing, a `.env` file for secrets, and a small pile of shell scripts holding it all together. PM2 has 42k+ GitHub stars and over a decade of production use. Nginx powers a huge slice of the internet. The combo has been the quiet default for self-hosted Node.js for years, and plenty of production apps still run on it today.

Tako collapses that stack into a single CLI. We think the philosophy is still right — a VPS and a good tool should be enough — but the shape of "a good tool" can be a lot more cohesive.

## At a glance

|                        | **PM2 + Nginx**                         | **Tako**                                               |
| ---------------------- | --------------------------------------- | ------------------------------------------------------ |
| **Number of tools**    | PM2, Nginx, certbot, shell scripts      | `tako` CLI + `tako-server`                             |
| **Proxy**              | Nginx (C)                               | Pingora (Rust, Cloudflare)                             |
| **Process manager**    | PM2                                     | Built into `tako-server`                               |
| **TLS**                | certbot + Let's Encrypt, manual renewal | Built-in ACME ([`tako.toml`](/docs/tako-toml))         |
| **Config**             | `ecosystem.config.js` + `nginx.conf`    | TOML ([`tako.toml`](/docs/tako-toml))                  |
| **Deploys**            | rsync / git pull + shell scripts        | `tako deploy` → SFTP + rolling restart                 |
| **Zero-downtime**      | `pm2 reload` (processes only)           | Pingora-coordinated rolling restart                    |
| **Secrets**            | `.env` files on disk                    | AES-256-GCM, delivered via fd 3                        |
| **Scale-to-zero**      | No                                      | Yes, with cold start                                   |
| **Local dev**          | None                                    | Built-in HTTPS + DNS ([`tako dev`](/docs/development)) |
| **Workflows / queues** | None (BYO Redis + BullMQ)               | Built into `tako-server`                               |
| **Multi-server**       | Script it yourself                      | Declarative per-environment                            |
| **Stars**              | PM2 ~42k                                | New kid on the block                                   |

## Where PM2 + Nginx shines

This stack has earned its longevity. PM2 does one job very well: it keeps your Node process alive, restarts on crash, runs in cluster mode to use every core, and gives you `pm2 reload` for zero-downtime process restarts. The CLI is fast, the logs are readable, and you can inspect a running app in seconds.

Nginx is Nginx. It's been the reference reverse proxy for over two decades. TLS termination is battle-tested, and `certbot --nginx` wires up Let's Encrypt in one command. If you know your way around `/etc/nginx/sites-available`, you can route any set of domains to any set of upstreams with surgical precision.

The combo also has the virtue of familiarity. Any senior backend engineer has touched both tools. Tutorials are everywhere. Debugging is well-trodden territory. For a single app on a single server, there's a lot to like.

## Where Tako is different

### One binary instead of four tools

The DIY stack is really four or five tools glued together: PM2 for processes, Nginx for TLS and routing, certbot for certificates, a `.env` file for secrets, and a shell script (or Makefile, or GitHub Action) to tie it all together. Each has its own config format, its own upgrade cadence, and its own failure modes. Nothing enforces that they agree — a new domain in `nginx.conf` doesn't automatically show up in your deploy script, and a new env var in `.env` doesn't automatically reach PM2.

Tako is a single Rust binary on the server (`tako-server`) and a single CLI on your laptop (`tako`). [`tako.toml`](/docs/tako-toml) declares processes, routes, TLS, and scaling in one file. The Pingora proxy, the process supervisor, secrets, and rolling restarts are coordinated by the same binary. There's nothing to glue together.

### A proxy that knows about deploys

Nginx terminates TLS and reverse-proxies requests; it doesn't know anything about your deployment. Zero-downtime updates mean orchestrating from outside: spin up new processes on new ports, health-check them, rewrite `upstream` blocks, `nginx -s reload`, then kill the old processes. `pm2 reload` handles the process side, but the proxy-side coordination is on you.

Tako uses [Pingora](/blog/pingora-vs-caddy-vs-traefik) — Cloudflare's Rust proxy framework — and the proxy is aware of the deploy state. New instances come up and register themselves; Pingora health-checks them, shifts traffic, then drains the old ones. No config reload, no race conditions, no scripts.

```d2
direction: right

diy: PM2 + Nginx {
  direction: down
  scripts: Deploy scripts
  pm2: PM2
  nginx: Nginx
  certbot: certbot

  scripts -> pm2: restart procs
  scripts -> nginx: rewrite config
  certbot -> nginx: renew certs
}

tako: Tako {
  direction: down
  cli: tako CLI
  server: tako-server

  cli -> server: SFTP + deploy
  server -> server: Pingora + procs + TLS
}
```

### Secrets aren't env vars

The standard pattern is a `.env` file on the server, read into `process.env` at startup. It works, but `.env` sits on disk, and environment variables inherit into any subprocess your app spawns — including children you didn't write.

Tako encrypts secrets locally with AES-256-GCM, ships them to the server where they live in an encrypted SQLite store, and hands them to each instance through [file descriptor 3](/blog/secrets-without-env-files) at spawn time. They never touch disk on the server and they don't leak to subprocesses.

### Scale-to-zero

PM2 keeps every registered process running. That's the right default for always-on apps, but if you're running staging environments, internal dashboards, and a few low-traffic side projects on the same VPS, they all eat memory 24/7.

Tako supports [on-demand scaling](/docs/how-tako-works): apps spin down after an idle timeout and cold-start on the next request. On a single [$5 VPS running multiple apps](/blog/your-5-dollar-vps-is-more-powerful-than-you-think), that's real memory savings.

### Local dev that matches production

PM2 is a production tool. Local dev is usually `npm run dev` plus whatever your framework hands you, and HTTPS during development is somebody else's problem.

[`tako dev`](/docs/development) runs your app with real HTTPS, local DNS (`*.test`), and the same SDK and process model that run in production. What works locally works the same way when deployed.

### Workflows and queues, not just processes

PM2 runs whatever process you give it. If your app needs durable background jobs, you wire up Redis + BullMQ (or Sidekiq-style equivalents) yourself. If you want WebSockets at scale, that's another service.

Tako ships [durable workflows](/blog/durable-workflows-are-here) inside `tako-server`, with [scale-to-zero workers](/blog/workflow-workers-scale-to-zero) so idle queues cost nothing. [Durable channels](/blog/durable-channels-built-in), queues, and image optimization follow the same idea — all in the same binary that's already running your app.

## Different ambitions

PM2 + Nginx is a stack you assemble. Each tool is excellent at its one job, and the seams between them are yours to own. For a seasoned engineer running one app on one VPS, it's still a reasonable answer — and it'll keep working for years.

Tako is a platform. Today that's deployment, routing, TLS, secrets, workflows, and local dev, all in one binary. Tomorrow it's channels, queues, and image optimization. Combined with [multi-server environments](/docs/deployment) and Cloudflare smart routing, you can [build your own edge network](/blog/build-your-own-edge-network-on-commodity-hardware) on cheap VPS boxes worldwide.

The question isn't whether PM2 + Nginx works — it does. The question is how many seams you want to own, and what you want your infrastructure to do on top of just running the process. Check out [how Tako works](/docs/how-tako-works) to see the architecture, or the [CLI docs](/docs/cli) to give it a try.
