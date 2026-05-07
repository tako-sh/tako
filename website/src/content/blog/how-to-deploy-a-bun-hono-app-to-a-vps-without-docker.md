---
title: "How to Deploy a Bun Hono App to a VPS Without Docker"
date: "2026-05-03T13:18"
description: "A literal Bun + Hono walkthrough: export app.fetch, run tako init, and ship to a VPS with HTTPS and rolling deploys. No Dockerfile required."
image: ad66c5107af2
---

[Hono](https://hono.dev) is a tiny web framework with a very useful property: a Hono app already speaks the web `fetch` shape. On Bun, that means your server can be one file that exports `fetch: app.fetch`. On [Tako](/docs), that also means your deploy target is just a Bun process behind Pingora, TLS, health checks, and rolling updates.

No Dockerfile. No image registry. No Nginx config. Let's walk the whole thing from `app.fetch` to `tako deploy`.

## Step 1 - Build the Hono app

Start with a plain Bun project:

```bash
mkdir hono-on-tako
cd hono-on-tako
bun init -y
bun add hono
```

Create `src/index.ts`:

```typescript
import { Hono } from "hono";

const app = new Hono();

app.get("/", (c) => c.text("Hello from Hono on Tako"));

app.get("/api/health", (c) =>
  c.json({
    ok: true,
    runtime: "bun",
  }),
);

export default {
  port: Number(process.env.PORT ?? 3000),
  fetch: app.fetch,
};
```

That final export is the whole trick. Hono's `app.fetch` is the request handler. Bun can run that object directly for local smoke tests, and Tako's JavaScript SDK can import the same module, grab its `fetch` function, and run it under the port Tako chooses for the process.

Add a script to `package.json` if you want a direct Bun run command:

```json
{
  "scripts": {
    "dev": "bun --hot src/index.ts"
  }
}
```

Then check it:

```bash
bun run dev
curl http://localhost:3000/api/health
```

You now have a Hono API that is already shaped like a deployable Tako app. The [fetch handler pattern](/blog/the-fetch-handler-pattern) is doing the heavy lifting here: `Request` in, `Response` out, no framework adapter required.

## Step 2 - Install Tako and prepare the VPS

On your laptop, install the CLI:

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

On the VPS, install `tako-server` as root:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

The server installer creates the `tako` service user, installs the `tako-server` binary, registers the service, prepares `/opt/tako`, and gives the proxy permission to bind ports 80 and 443. That one server process owns routing, ACME certificates, process supervision, rolling updates, and the encrypted secrets store. The [deployment guide](/docs/deployment) has the longer day-two version; for this tutorial, the installer is enough.

Point a DNS A record at the VPS before the first deploy:

| Thing            | Example                          |
| ---------------- | -------------------------------- |
| VPS public IP    | `203.0.113.10`                   |
| DNS record       | `api.example.com A 203.0.113.10` |
| Tako server name | `prod`                           |
| Tako route       | `api.example.com`                |

Back on your laptop, register the server once:

```bash
tako servers add 203.0.113.10 --name prod
```

`tako servers add` verifies SSH, detects the server target, and stores that server in your global Tako config. Future projects can reuse the same server name.

## Step 3 - Run `tako init`

Inside the Hono project:

```bash
tako init
```

Init detects Bun from the project, writes `tako.toml`, updates `.gitignore`, pins your local Bun runtime version when it can, and installs the `tako.sh` SDK with Bun. For a small Hono app, keep the config explicit:

```toml
name = "hono-on-tako"
runtime = "bun"
package_manager = "bun"
main = "src/index.ts"

[envs.production]
route = "api.example.com"
servers = ["prod"]
```

There is no Hono preset because Hono does not need one. Presets are useful when a framework needs build output normalization, assets, or a special dev command. Hono is already a fetch handler, so `main = "src/index.ts"` is enough. The [framework guide](/docs/framework-guides#fallback-fetch-handler-no-preset) calls this the fallback fetch-handler path, but for Hono it is the natural path.

If your API has a build step, add it. If it does not, leave it out:

```toml
[build]
run = "bun run build"
```

For a simple Bun API that runs TypeScript directly, you usually do not need that block. `tako deploy` will still package your source, upload it, run a production dependency install on the server, and launch the configured `main` under Bun.

## Step 4 - Test the same shape locally

Before deploying, run the app through Tako:

```bash
tako dev
```

This is not just a convenience wrapper around `bun run dev`. For JavaScript apps, `tako dev` uses the same SDK entrypoint shape as production: it imports your `main`, wraps the fetch handler, exposes the built-in status endpoint, and reports the bound port back to the local Tako daemon. The local proxy then serves the app on a `.test` hostname with HTTPS.

For this project you should see a route like:

```text
https://hono-on-tako.test/
```

That local HTTPS path is useful for OAuth callbacks, secure cookies, service workers, and any code that behaves differently on plain `http://localhost`. The [development docs](/docs/development) cover the local proxy, DNS, and LAN mode pieces.

## Step 5 - Deploy

Run:

```bash
tako deploy
```

Confirm the production prompt, then watch the task tree:

```text
Connecting     ✓
Building       ✓
Deploying to prod
  Uploading    ✓
  Preparing    ✓
  Starting     ✓

  https://api.example.com/
```

Open `https://api.example.com/api/health`. The first deploy issues a Let's Encrypt certificate automatically for the public route, starts one Bun instance, waits for the SDK readiness signal, and only then sends traffic to it.

On the server, the app is not a container. It is a native Bun process. Tako launches the Bun runtime entrypoint from `tako.sh`, imports `src/index.ts`, extracts the default `fetch` function from your Hono export, and serves it on `127.0.0.1` with an assigned port. The process reports that port back to `tako-server`; Pingora terminates HTTPS on `:443` and routes requests to the healthy instance.

```d2
direction: right

local: "Laptop" {
  code: "src/index.ts\nHono app.fetch"
}

artifact: ".tar.zst artifact" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

server: "VPS" {
  proxy: "Pingora proxy\nHTTPS :443" {
    style.fill: "#E88783"
  }

  bun: "Bun process\nTako SDK + Hono fetch" {
    style.fill: "#9BC4B6"
  }

  proxy -> bun: "Request -> Response"
}

local.code -> artifact: "tako deploy packages"
artifact -> server: "SFTP upload"
```

That same flow is what gives you rolling deploys. On the next `tako deploy`, each server starts a new instance, waits for it to become healthy, adds it to the load balancer, drains an old instance, then moves the `current` symlink to the new release. If the new process cannot start, the old release keeps serving. The [CLI reference](/docs/cli#tako-deploy) lists the flags, and [the rolling update section](/docs/deployment#rolling-updates) explains the production behavior.

## What you did not need

The Hono app is already the server interface Tako wants, so the deployment stack stays small:

| Usual VPS chore             | What happens here                                                             |
| --------------------------- | ----------------------------------------------------------------------------- |
| Write a `Dockerfile`        | Skip it; Bun runs the app as a native process                                 |
| Push an image to a registry | Skip it; Tako uploads a deploy artifact over SFTP                             |
| Configure Nginx and Certbot | Skip it; Pingora and ACME live in `tako-server`                               |
| Hand-roll restart scripts   | Skip it; deploys are health-checked rolling updates                           |
| Copy `.env` files around    | Use [`tako secrets`](/docs/cli#tako-secrets) when you need production secrets |

This is why Hono is such a clean fit for Tako. The app code stays portable: remove Tako later and the handler still works on Bun. While it runs on Tako, you get the platform pieces around it: HTTPS, routing, logs, deploy history, rollbacks, secrets, and scaling commands.

Start with one endpoint and one VPS. When the app grows, add [multiple environments or servers](/docs/deployment#configure-the-project), scale the desired instance count with `tako scale`, and keep shipping with the same `tako deploy`.
