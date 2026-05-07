---
title: Hello, world
date: "2026-03-17T12:00"
author: dan
description: "Tako is live — a complete platform for running apps on your own servers. Deployment, routing, TLS, secrets, and local dev in a single tool."
image: 8d73afeb3c9d
---

Tako is live. I've been building it for some time and I'm finally ready to share it.

## What is Tako?

Tako is a complete platform for running your apps on your own servers. Deployment, routing, TLS, secrets, logs, rolling updates, local development — all handled by a single tool. You own the server, you own the process. No vendor lock-in, no black boxes.

Think of it as your own self-hosted cloud, minus the cloud.

## Why I built it

Cloud platforms are great until they aren't. Pricing surprises, cold starts, opaque infrastructure, and the feeling that you're renting your own app from someone else.

I wanted something simpler:

- **One command to deploy** — `tako deploy` and you're done
- **Self-hosted** — runs on any Linux server you control
- **Zero downtime** — rolling updates with health checks
- **Minimal config** — a `tako.toml` and you're set

## Give it a try

Getting started takes a minute. Install Tako, create a `tako.toml`, and deploy:

```bash
curl -fsSL https://tako.sh/install.sh | sh
tako init
tako deploy
```

The [quick start guide](/docs) walks you through everything step by step.

## What's next

I'm working on making Tako even easier to get started with. Follow along here or [on 𝕏](https://twitter.com/intent/follow?screen_name=lilienblum) for updates.
