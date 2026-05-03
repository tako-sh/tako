---
title: "Deploy TanStack Start to a VPS in Five Minutes"
date: "2026-04-29T12:44"
description: "A concrete walkthrough: scaffold a TanStack Start app, run tako init and tako deploy, and watch the SSR bundle boot natively on Bun behind Pingora — no Docker, no edge platform."
image: fd31b5e47c64
---

[TanStack Start](https://tanstack.com/start/latest) is a full-stack React framework with file-based routing, server functions, and a real SSR build. Most tutorials for it end with "now deploy to Netlify / Vercel / Cloudflare Workers." Those work, but they're not your only option. The SSR output is a Node-compatible fetch handler — which means a single Linux box with [Tako](/docs) on it is enough.

Here's the whole walkthrough. Total wall-clock time, assuming a server is already provisioned: about five minutes.

## Step 1 — Scaffold the app

```bash
npx @tanstack/cli@latest create my-app
cd my-app
```

Pick Bun as the package manager when the wizard asks (Node works too — Tako supports both). Add Tailwind or whatever else you want. The default starter is a working SSR app right out of the box.

Add the [Tako Vite plugin](/docs/framework-guides) so the build emits a server entry Tako can launch:

```ts
// vite.config.ts
import { defineConfig } from "vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import { tako } from "tako.sh/vite";

export default defineConfig({
  plugins: [tanstackStart(), tako()],
});
```

That's the only framework-side change. The plugin doesn't replace TanStack Start's build — it adds a thin wrapper at `dist/server/tako-entry.mjs` that re-exports the SSR fetch handler in [the shape Tako expects](/blog/the-fetch-handler-pattern).

## Step 2 — `tako init`

```bash
tako init
```

Init is interactive. It detects Bun from your lockfile, sees `@tanstack/react-start` in `package.json`, and offers `tanstack-start` as the preset. Accept the defaults and you'll get a `tako.toml` like this:

```toml
name = "my-app"
runtime = "bun"
runtime_version = "1.2.x"
preset = "tanstack-start"

[envs.production]
route = "my-app.example.com"
servers = ["prod"]
```

The [`tanstack-start` preset](/docs/presets) bakes in `main = "dist/server/tako-entry.mjs"` and `assets = ["dist/client"]`, so you don't write either. Init also runs `bun add tako.sh` so the SDK is in your dependencies, and updates `.gitignore` so `.tako/*` is ignored while `.tako/secrets.json` stays tracked.

Change `route` to a domain you actually own. Tako will issue a Let's Encrypt cert for it on first deploy.

## Step 3 — `tako deploy`

If you don't have a server registered yet, do that once:

```bash
tako servers add prod.example.com --name prod
```

This connects as the `tako` user, detects `arch` and `libc`, and writes the entry to your global `config.toml`. ([How to install `tako-server` on the box](/docs/deployment) is in the deploy docs — `apt install tako-server` on Debian / Ubuntu, equivalent on Alpine.)

Then:

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

Open the URL. Your TanStack Start app is live, with a real cert, behind [Pingora](/blog/pingora-vs-caddy-vs-traefik), running natively on Bun.

## What just happened

```d2
direction: right

local: Your laptop {
  build: "vite build\n+ tako-entry.mjs"
}

artifact: ".tar.zst\nartifact" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

server: VPS {
  proxy: "Pingora\n(:443, TLS)" {
    style.fill: "#E88783"
  }
  bun: "Bun process\n(SSR handler)" {
    style.fill: "#9BC4B6"
  }
  proxy -> bun: "fetch()"
}

local.build -> artifact: "package"
artifact -> server: "SFTP"
```

The deploy ran [`vite build`](/docs/deployment) locally, packaged everything (excluding `.git`, `.tako`, `.env*`, and `node_modules`) into a `.tar.zst` artifact, and SFTP'd it to your server. `tako-server` unpacked it to `/opt/tako/my-app/releases/{version}/`, ran a production install, and started the SSR bundle directly under Bun. Pingora terminates TLS on `:443`, the per-app load balancer routes to your process, and the SDK answers Tako's internal health checks so unhealthy instances drop out automatically.

There is no container, no Node-on-Bun shim, no Lambda cold start. It's a Linux process started by a service manager on a VPS — but you got there with two commands.

## Why this matters

TanStack Start runs anywhere a fetch handler runs: Vercel, Netlify, Cloudflare Workers, Bun on a box. The hosted platforms are the loudest option in the room, and they're great at what they do — but they aren't the only option, and the lock-in tradeoff is real. With Tako you keep the same SSR app and the same fetch-handler interface, deployed to hardware you control, with [zero-downtime rolling updates](/blog/scale-to-zero-without-containers) and [HTTPS in local dev](/blog/local-dev-with-real-https) thrown in.

Five minutes from `create-start-app` to live HTTPS. Try the rest of the [CLI reference](/docs/cli) when you want secrets, multi-server routing, or rollbacks.
