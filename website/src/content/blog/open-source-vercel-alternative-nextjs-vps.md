---
title: "The Open Source Vercel Alternative for Next.js Apps on a VPS"
date: "2026-05-07T01:22"
description: "Compare Vercel's hosted Next.js path with Tako's open-source VPS path: standalone output, one adapter, owned servers, and rolling deploys."
image: 0feaccc1b663
---

Vercel is the obvious answer for Next.js because it is a very good answer. The framework and the platform grew up together. You push to Git, get preview URLs, merge to production, and the deployment model understands Next.js deeply.

That is hard to beat if your goal is "think about servers as little as possible."

But sometimes the goal changes. You already have a VPS. You want a flat bill. You want your app, logs, secrets, data directory, and deploy history on hardware you control. You still want the nice Next.js path, but you do not want the whole app to live inside a hosted platform account forever.

That is where [Tako](/docs/) fits: not a Vercel clone, and not a dashboard PaaS. It is an [open-source](https://github.com/tako-sh/tako) deploy and runtime layer for your own server. For a Next.js app, the interesting part is that Next.js already has the pieces needed to run outside Vercel cleanly.

## The real tradeoff

The search phrase is "open source Vercel alternative," but the better question is narrower: where should this Next.js app run?

| Question              | Vercel                                                                              | Tako                                                                              |
| --------------------- | ----------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| Who owns the runtime? | Vercel                                                                              | You, on your VPS                                                                  |
| Deploy input          | Git push, CLI, hooks, or API                                                        | Local build artifact over SFTP                                                    |
| Next.js integration   | First-party hosted platform from the creators of Next.js                            | `withTako()` adapter plus the `nextjs` preset                                     |
| Runtime shape         | Vercel-managed infrastructure for static assets, functions, and framework features  | Native Node or Bun process behind Pingora                                         |
| Local development     | Vercel CLI and standard framework dev tools                                         | [`tako dev`](/docs/development/) with local HTTPS, DNS, and proxy                 |
| Rollouts              | Managed by Vercel                                                                   | [`tako deploy`](/docs/deployment/) rolling update with health checks and rollback |
| Best fit              | Zero infrastructure, preview-heavy team workflows, global managed frontend platform | Owned server, predictable infrastructure, backend primitives next to the app      |

Vercel's own [Git deployment docs](https://vercel.com/docs/deployments/git) describe automatic deployments from Git, preview deployments for pull requests, production deployments from the production branch, and instant rollback when a custom-domain deployment is reverted. That flow is excellent. It is a product choice as much as a technical choice: source control is the deploy interface, and the platform owns the rest.

Tako makes a different choice. Your laptop builds the app, packages the output, ships it to the server, and asks `tako-server` to roll it forward. The server owns TLS, routing, process supervision, secrets, release history, and scale. The deploy surface is still one command, but the machine is yours.

## Next.js is already portable

The reason this comparison is even possible: Next.js is not only "the thing you deploy to Vercel." The current Next.js [deploying docs](https://nextjs.org/docs/app/getting-started/deploying) list several deployment targets, including a Node.js server, Docker, static export, and adapters. For a full server-rendered app, the boring Node server path still matters.

The key setting is `output: "standalone"`. Next.js traces the files needed at runtime and writes a `.next/standalone` directory with a minimal `server.js`. The official [output docs](https://nextjs.org/docs/app/api-reference/config/next-config-js/output) call this out as a production deployment shape: the standalone folder can be deployed without the full `node_modules`, and `node .next/standalone/server.js` starts the app.

Tako wraps that path instead of inventing a parallel Next.js runtime.

```ts
// next.config.ts
import { withTako } from "tako.sh/nextjs";

export default withTako({});
```

That helper does three things that line up with Next.js's own deployment hooks:

| `withTako()` behavior                                                                             | Why it exists                                           |
| ------------------------------------------------------------------------------------------------- | ------------------------------------------------------- |
| Sets `output: "standalone"`                                                                       | Build the minimal Next.js server output Tako can launch |
| Sets [`adapterPath`](https://nextjs.org/docs/app/api-reference/config/next-config-js/adapterPath) | Let the Tako adapter run during the Next build          |
| Adds `*.test` and `*.tako.test` to `allowedDevOrigins`                                            | Let local HTTPS dev hostnames reach `next dev`          |

On build, the adapter writes `.next/tako-entry.mjs`. That entry file prefers `.next/standalone/server.js` when Next emits it. If standalone output is missing for the current pipeline, it falls back to `next start` against the built `.next/` directory and installed `next` package.

Then `tako.toml` stays short:

```toml
runtime = "node"
preset = "nextjs"

[envs.production]
route = "app.example.com"
servers = ["prod"]
```

The [`nextjs` preset](/docs/presets/) supplies `main = ".next/tako-entry.mjs"` and `dev = ["next", "dev"]`, so you are not hand-maintaining an entrypoint path. The [framework guide](/docs/framework-guides/#nextjs) has the small setup version; this post is about why that setup changes the deployment decision.

```d2
direction: right

next: "Next.js app" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

vercel: "Vercel path" {
  git: "git push"
  platform: "managed build\n+ frontend cloud"
  runtime: "static assets\n+ functions"
  git -> platform -> runtime
  style.fill: "#E88783"
}

tako: "Tako path" {
  build: "next build\n+ tako-entry.mjs"
  artifact: "artifact"
  server: "your VPS\nPingora + Node/Bun"
  build -> artifact -> server
  style.fill: "#9BC4B6"
}

next -> vercel.git
next -> tako.build
```

## What the VPS path gives back

The obvious reason to pick Vercel is that you do not have to own the platform. The obvious reason to pick Tako is that you want to.

That ownership shows up in a few practical places.

First, deploys are normal server events, not platform events. `tako deploy` builds locally, uploads a versioned artifact, runs production install on the server, starts a fresh instance, waits for the internal health check, adds it to the load balancer, and drains the old one. If startup fails, the old instance keeps serving and the failed release is cleaned up. You can inspect release history with `tako releases list` and roll back with `tako releases rollback`.

Second, secrets are part of the platform instead of scattered `.env` files. [`tako secrets set`](/docs/cli/) stores encrypted project secrets locally, syncs them to mapped servers, and `tako-server` injects them into app processes without writing a release `.env` file. For Next.js apps, `tako.sh` gives typed runtime state and generated `tako.d.ts` declarations add typed secrets, so server code can import what it needs without leaning on untyped `process.env` reads.

Third, the backend pieces are moving closer to the app. Tako's realtime model uses [durable channels](/blog/durable-channels-built-in/) for WebSocket/SSE traffic with bounded replay, alongside [durable workflows](/blog/durable-workflows-are-here/) for retries, cron, sleeps, and `signal` / `waitFor`. A Next.js route can enqueue a workflow or publish a realtime event once the server runtime is initialized. That means a self-hosted Next.js app can grow realtime and background work without immediately buying three more services.

Fourth, local dev uses the same philosophy as production. [`tako dev`](/docs/development/) gives you real local HTTPS, `.test` DNS, and a local proxy. The Next.js adapter adds the allowed dev origins so `next dev` accepts those hostnames. You are not testing `localhost:3000` and hoping production behaves like a routed HTTPS app later.

None of this means "never use Vercel." It means the tradeoff is real now. The easy path is not only hosted anymore.

## Where Vercel still wins

Vercel remains the best default for many Next.js teams. If you want preview deployments for every pull request, dashboard-first collaboration, a global managed frontend network, and no SSH key anywhere near the team, Vercel is built for that. It is especially strong when the app is mostly frontend, the team values managed workflow over server ownership, and the bill is comfortably worth the saved operations time.

Tako is for the other shape:

| Pick this        | When                                                                       |
| ---------------- | -------------------------------------------------------------------------- |
| Vercel           | You want the managed Next.js platform and do not want to operate servers   |
| Tako             | You want a Vercel-like deploy feel on hardware you control                 |
| Docker on a VPS  | You want container isolation and already have registry/deploy plumbing     |
| Raw Node + Nginx | You want full manual control and do not mind stitching the pieces yourself |

The important word is "alternative," not "drop-in replacement." Vercel is a hosted product with a lot of surface area: previews, analytics, observability, marketplace integrations, enterprise controls, and a global edge story. Tako is an open-source platform layer for your server: CLI, proxy, TLS, secrets, deploys, process management, local dev, and app-level primitives.

If your app needs Vercel's hosted workflow, use it. It is good.

If your app is a Next.js server that should live on your own VPS, Tako gives it a clean path: one adapter in `next.config.ts`, one `preset = "nextjs"` in [`tako.toml`](/docs/tako-toml/), and one deploy command.

Same framework. Different owner.

[Read the Next.js framework guide →](/docs/framework-guides/#nextjs)
