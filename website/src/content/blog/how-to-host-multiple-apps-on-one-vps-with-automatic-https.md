---
title: "How to Host Multiple Apps on One VPS with Automatic HTTPS"
date: "2026-05-08T02:24"
description: "Run several apps on one VPS with Tako routes, SNI certificate selection, static assets, and automatic HTTPS for every domain."
image: 97424f86ce07
---

One VPS is enough for more apps than people give it credit for.

The hard part is not CPU. The hard part is the pile of glue around the apps: one Nginx config per hostname, one process manager stanza per service, one Certbot renewal path, one static-file exception, one "why is `/api` hitting the wrong app?" debugging session. Do that three times and your small server starts feeling like a tiny operations department.

Tako's model is simpler: every app owns its own [`tako.toml`](/docs/tako-toml), every environment declares the routes it wants, and `tako-server` builds one route table across the box. Pingora terminates HTTPS on `:443`, selects the certificate by SNI, matches the request host/path, serves static assets when it can, and forwards the rest to the right app process.

This walkthrough hosts three apps on one VPS:

| App    | Route                                  | Job                          |
| ------ | -------------------------------------- | ---------------------------- |
| `www`  | `example.com`, `www.example.com`       | Marketing site               |
| `api`  | `api.example.com`, `example.com/api/*` | HTTP API                     |
| `docs` | `example.com/docs/*`                   | Docs app under a path prefix |

One server. Three deployments. Automatic HTTPS for the public hostnames.

## What you need

You need a Linux VPS, a domain, the local [`tako` CLI](/docs/cli), and `tako-server` installed on the box. The server installer sets up the Rust server binary, the service manager unit, the `tako` control user, the `tako-app` runtime user, port binding capabilities, and the Pingora proxy.

On your laptop:

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

On the VPS:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

Then register the server once:

```bash
tako servers add prod.example-tailnet.ts.net --name prod
```

For public app traffic, point DNS at the VPS public IP:

| DNS record        | Value                          |
| ----------------- | ------------------------------ |
| `example.com`     | `A` / `AAAA` record to the VPS |
| `www.example.com` | `A` / `AAAA` record to the VPS |
| `api.example.com` | `A` / `AAAA` record to the VPS |

You do not need one server per app. You do not need one reverse-proxy config per hostname. The app route declarations become the proxy config.

## Give each app a stable identity

Each app gets its own project directory and its own `tako.toml`. Set `name` explicitly. Tako can infer a name from the directory, but top-level `name` is the stable server-side identity. A production deploy of `www` lives under the app identity `www/production`; a production deploy of `api` lives under `api/production`.

That separation matters on a shared box:

| Piece                    | Separated by app identity? |
| ------------------------ | -------------------------- |
| Release directories      | Yes                        |
| Runtime processes        | Yes                        |
| Routes                   | Yes                        |
| Secrets                  | Yes                        |
| `TAKO_DATA_DIR` app data | Yes                        |
| Scale setting            | Yes                        |

The marketing app can be a Next.js app:

```toml
# apps/www/tako.toml
name = "www"
runtime = "node"
preset = "nextjs"

[envs.production]
routes = ["example.com", "www.example.com"]
servers = ["prod"]
```

The API can be a Bun app:

```toml
# apps/api/tako.toml
name = "api"
runtime = "bun"

[envs.production]
routes = ["api.example.com", "example.com/api/*"]
servers = ["prod"]
```

The docs app can live under a path prefix:

```toml
# apps/docs/tako.toml
name = "docs"
runtime = "bun"
preset = "vite"
assets = ["dist/client"]

[build]
run = "bun run build"

[envs.production]
route = "example.com/docs/*"
servers = ["prod"]
```

Deploy them independently:

```bash
cd apps/www && tako deploy
cd ../api && tako deploy
cd ../docs && tako deploy
```

Each deploy updates one app. The others keep serving. If you ship a docs typo, the API does not restart. If you rotate API secrets, the marketing app does not care.

## How route matching works

At runtime, Tako has one route table per server. Every deployed app contributes its routes. Incoming requests are matched by host and path, then sent to the selected app's load balancer.

```d2
direction: right

browser: Browser

proxy: "Pingora proxy\n:443" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

table: "Tako route table" {
  style.fill: "#9BC4B6"
}

www: "www app\nexample.com"
api: "api app\napi.example.com\nexample.com/api/*"
docs: "docs app\nexample.com/docs/*"

browser -> proxy: "HTTPS request"
proxy -> table: "host + path"
table -> www: "example.com/"
table -> api: "example.com/api/users"
table -> docs: "example.com/docs/start"
```

Specific routes win. Exact host beats wildcard host. Longer path beats shorter path. A host-only route like `example.com` can serve normal page traffic, while `example.com/api/*` and `example.com/docs/*` carve out subtrees for other apps.

| Request                          | Selected app | Why                                               |
| -------------------------------- | ------------ | ------------------------------------------------- |
| `https://example.com/`           | `www`        | Host-only route matches                           |
| `https://www.example.com/`       | `www`        | Exact hostname route matches                      |
| `https://api.example.com/users`  | `api`        | Exact API hostname route matches                  |
| `https://example.com/api/users`  | `api`        | Longer `/api/*` path route beats host-only route  |
| `https://example.com/docs/start` | `docs`       | Longer `/docs/*` path route beats host-only route |

Tako validates this at deploy time. Routes must include a hostname. A non-development environment must define `route` or `routes`. A single environment can use `route` for one route or `routes` for many, but not both. Deploy conflict detection prevents overlapping routes from silently stealing traffic.

The practical rule: use exact hostnames when you can, path prefixes when you want one apex domain to feel like several apps, and wildcard routes only when the app really owns tenant subdomains.

## Static assets under path prefixes

Static assets are where path-prefix hosting usually gets annoying. A docs bundle might emit `/assets/main.js`, but visitors request it as `/docs/assets/main.js` because the app is mounted under `example.com/docs/*`.

Tako handles that in the proxy. For static asset requests, `tako-server` looks in the deployed app's `public/` directory. When the matched route has a path prefix, it also tries the prefix-stripped path.

| Request                | Matched route        | Static lookup candidates                       |
| ---------------------- | -------------------- | ---------------------------------------------- |
| `/docs/assets/main.js` | `example.com/docs/*` | `/docs/assets/main.js`, then `/assets/main.js` |
| `/docs/logo.png`       | `example.com/docs/*` | `/docs/logo.png`, then `/logo.png`             |

Your app keeps its normal build output, while Tako makes subpath deployment work at the edge. If no static file exists, the request falls through to the app process.

This is also why the `assets` field matters. Presets can provide default asset roots, and top-level `assets` can add more. During deploy, those assets are merged into the app's deployed `public/` directory, where the proxy can serve them directly before waking or forwarding to the app.

## Automatic HTTPS per route

When you deploy a public route, Tako asks for the certificate it needs. For normal public hostnames, it uses ACME with Let's Encrypt and the HTTP-01 challenge on port 80. Let's Encrypt's challenge docs describe HTTP-01 as a token served from `/.well-known/acme-challenge/<TOKEN>` on port 80; Tako's proxy handles that challenge path before app routing.

At TLS handshake time, the browser sends SNI for the hostname. Tako looks up the matching certificate, tries wildcard fallback when appropriate, and serves a fallback self-signed certificate only when no matching certificate exists yet so the connection can still complete and return a normal HTTP status.

For the three-app setup:

| Route                | Certificate behavior               |
| -------------------- | ---------------------------------- |
| `example.com`        | Public certificate via HTTP-01     |
| `www.example.com`    | Public certificate via HTTP-01     |
| `api.example.com`    | Public certificate via HTTP-01     |
| `example.com/docs/*` | Uses the `example.com` certificate |

Wildcard routes are the special case. If you deploy `*.example.com`, HTTP-01 cannot prove control of every possible subdomain. Tako supports wildcard certificates through Cloudflare DNS-01. Configure Cloudflare DNS credentials first:

```bash
tako dns configure --env production
```

Then deploy the wildcard route. If the app environment is missing Cloudflare DNS-01 credentials and declares a wildcard route, deploy fails with a setup hint instead of leaving you with a route that cannot get the right certificate.

## The box stays understandable

After the three deploys, `tako servers status` gives you the server view:

```text
✓ prod up
  ┌ www (production) running
  │ instances: 1/1
  └ deployed: ...
  ┌ api (production) running
  │ instances: 1/1
  └ deployed: ...
  ┌ docs (production) running
  │ instances: 1/1
  └ deployed: ...
```

You can scale each app separately:

```bash
cd apps/api
tako scale 2 --env production

cd ../docs
tako scale 0 --env production
```

The API can stay warm with two instances. The docs app can scale to zero and wake on request. Both decisions persist across deploys and server restarts because desired instance count is server runtime state, not a line in `tako.toml`.

That is the point: the server stays one server, but the apps stay separate apps. Routes decide traffic. SNI decides certificates. App identity decides disk paths, secrets, logs, data, releases, and scaling.

Start with one app, then add the second one without changing the first. The [deployment docs](/docs/deployment) cover the full deploy flow, and [how Tako works](/docs/how-tako-works) explains the proxy/process architecture underneath.
