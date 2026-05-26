---
title: "How to Deploy Next.js to a VPS Without Docker"
date: "2026-04-29T13:00"
description: "A literal walkthrough — provision a $5 VPS, point a domain, and ship a Next.js app to it with tako init + tako deploy. HTTPS and zero-downtime rollouts, no Dockerfile in sight."
image: e67599ab52d4
---

The two paths most Next.js deploy tutorials show: push to Vercel, or write a Dockerfile and ship the image somewhere. The first is the easiest thing in the world — until you want to actually own the box. The second works, but you've signed up for Dockerfiles, multi-stage builds, image registries, and a `docker-compose.yml` to run a process that fundamentally just needs Node and a port.

There's a third path. A $5 VPS, a domain, and [Tako](/docs/). No container in sight.

## What you need

Five things. None of them include Docker.

| Thing                         | Where it comes from                                                                                                     |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| A VPS                         | [Hetzner CX22 (~$6/mo)](/blog/your-5-dollar-vps-is-more-powerful-than-you-think/), DigitalOcean, Vultr — anything Linux |
| A domain                      | Wherever; point an A record at the VPS IP                                                                               |
| `tako-server` on the box      | One curl command                                                                                                        |
| The `tako` CLI on your laptop | One curl command                                                                                                        |
| A Next.js app                 | `npx create-next-app@latest my-app`                                                                                     |

## Step 1 — Install the CLI and `tako-server`

On your laptop:

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

SSH into the VPS and run the server installer:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

That's the entire server-side setup. The installer drops a single Rust binary, registers a systemd unit, creates a non-root `tako` user, and grants it the capability to bind ports 80 and 443. Pingora proxy, ACME, process supervision, and the encrypted secrets store all live inside that one binary. The [deployment docs](/docs/deployment/) cover the details, but defaults work — skip them on the first pass.

## Step 2 — Wire up Next.js

There is exactly one line of Next-specific config. In your `next.config.ts`:

```ts
import { withTako } from "tako.sh/nextjs";

export default withTako({});
```

`withTako()` switches Next.js to standalone output, installs the Tako adapter, and lets `next dev` accept Tako's local HTTPS hostnames (`*.test`). On build it emits `.next/tako-entry.mjs` — the small file Tako launches in production. The [framework guide](/docs/framework-guides/#nextjs) has the full breakdown.

## Step 3 — `tako init`

In the project directory:

```bash
tako init
```

Init reads your `package.json`, sees `next`, and offers the `nextjs` preset. Accept the defaults and you'll get a `tako.toml` like this:

```toml
name = "my-app"
runtime = "node"
runtime_version = "22.x"
preset = "nextjs"

[envs.production]
route = "my-app.example.com"
servers = ["prod"]
```

Init also runs `npm add tako.sh` (or `bun add`, depending on your package manager) to drop the SDK in, and tunes `.gitignore` so `.tako/*` is ignored — except `.tako/secrets.json`, which stays tracked. The [`nextjs` preset](/docs/presets/) bakes in `main = ".next/tako-entry.mjs"` so you don't write that line yourself.

Change `route` to a domain you actually own, then register the server once:

```bash
tako servers add prod.example.com --name prod
```

## Step 4 — `tako deploy`

```bash
tako deploy
```

Confirm the production prompt and watch the task tree:

```
Connecting     ✓
Building       ✓
Deploying to prod
  Uploading    ✓
  Preparing    ✓
  Starting     ✓

  https://my-app.example.com/
```

Open the URL. Your Next.js app is live, on a real Let's Encrypt cert, with [zero-downtime rolling updates](/blog/scale-to-zero-without-containers/) on every subsequent `tako deploy`.

## What just happened

```d2
direction: right

local: Your laptop {
  build: "next build\n+ tako-entry.mjs"
}

artifact: ".tar.zst\nartifact" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

server: VPS {
  proxy: "Pingora\n(:443, TLS)" {
    style.fill: "#E88783"
  }
  node: "Node process\n(.next standalone)" {
    style.fill: "#9BC4B6"
  }
  proxy -> node: "fetch()"
}

local.build -> artifact: "package"
artifact -> server: "SFTP"
```

`tako deploy` ran `next build` locally, packaged the result (excluding `.git`, `.tako`, `.env*`, and `node_modules`) into a `.tar.zst` artifact, and SFTP'd it to the VPS. `tako-server` unpacked it under `/opt/tako/apps/my-app/production/`, ran a production install, and started the Next.js standalone server as a regular Node process. [Pingora](/blog/pingora-vs-caddy-vs-traefik/) terminates TLS on `:443` and routes to it.

## What you didn't write

- A `Dockerfile`
- A `docker-compose.yml`
- An `nginx.conf` and a `certbot` cron
- A `.env` file copied into a container at build time
- A GitHub Actions workflow that builds, tags, and pushes an image

That's the difference. Next.js doesn't need to be containerized to run; it's a Node program that listens on a port. Tako treats it like one. The proxy, the TLS, the secrets store, and the rolling-restart coordinator all live in a single Rust binary on the box. Your Next.js app is a process, not a container image.

When you're ready for more — [secrets that don't sit in env files](/blog/secrets-without-env-files/), [multiple environments on the same box](/blog/one-config-many-servers/), or [durable workflows from inside your routes](/blog/tako-workflows-in-nextjs-via-instrumentation/) — it's all the same `tako deploy`. Start with the [CLI reference](/docs/cli/) or skim [how Tako works](/docs/how-tako-works/).
