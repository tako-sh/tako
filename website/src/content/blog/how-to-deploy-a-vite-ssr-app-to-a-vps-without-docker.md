---
title: "How to Deploy a Vite SSR App to a VPS Without Docker"
date: "2026-05-06T07:43"
description: "Build a Vite React SSR app, wrap the server bundle with tako.sh/vite, ship dist/client assets, and deploy to a VPS as a native process."
image: f1cb5b380e9d
---

Vite's SSR story is refreshingly direct: make a browser build, make a server build, and run the server entry in production. Most tutorials finish by wrapping that in Express, a Dockerfile, or a hosted platform adapter.

You do not need the container layer for that. With [Tako](/docs), the server bundle can run as a normal Node or Bun process on a VPS, while Tako handles HTTPS, routing, health checks, static assets, and [zero-downtime deploys](/blog/zero-downtime-deploys-without-a-container-in-sight).

This walkthrough uses a plain Vite React SSR app, the `tako.sh/vite` plugin, and one explicit `tako.toml`. No Dockerfile, no image registry, no Nginx side quest.

## Step 1 - Create the Vite app

Start with the regular Vite React template:

```bash
npm create vite@latest vite-ssr-on-tako -- --template react-ts
cd vite-ssr-on-tako
npm install
npm install tako.sh
```

The default template is a client-side app. To make it SSR-shaped, change `index.html` so React has a server-rendered outlet:

```html
<div id="root"><!--ssr-outlet--></div>
<script type="module" src="/src/main.tsx"></script>
```

Then make the browser entry hydrate instead of creating a fresh client-only tree:

```tsx
// src/main.tsx
import { StrictMode } from "react";
import { hydrateRoot } from "react-dom/client";
import App from "./App";
import "./index.css";

hydrateRoot(
  document.getElementById("root")!,
  <StrictMode>
    <App />
  </StrictMode>,
);
```

Now add the server entry. The important part is the export: `tako.sh/vite` expects the compiled server module to expose a fetch handler, either as a default function, a default object with `.fetch`, or a named `fetch` export.

```tsx
// src/entry-server.tsx
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { renderToString } from "react-dom/server";
import App from "./App";

const serverDir = path.dirname(fileURLToPath(import.meta.url));
const templatePath = path.resolve(serverDir, "../client/index.html");

export default async function fetch(request: Request): Promise<Response> {
  const url = new URL(request.url);
  const template = await readFile(templatePath, "utf8");
  const appHtml = renderToString(<App />);

  const html = template
    .replace("<!--ssr-outlet-->", appHtml)
    .replace("<title>Vite + React + TS</title>", `<title>${url.pathname} - Vite SSR</title>`);

  return new Response(html, {
    headers: { "content-type": "text/html; charset=utf-8" },
  });
}
```

This is deliberately small. If your app uses React Router, TanStack Router, or another SSR router, pass `url.pathname` into that router instead of rendering the same `<App />` for every path. The deployment shape stays the same: `Request` in, `Response` out. That [fetch handler pattern](/blog/the-fetch-handler-pattern) is the boundary Tako runs.

## Step 2 - Add the Tako Vite plugin

Update `vite.config.ts`:

```ts
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import { tako } from "tako.sh/vite";

export default defineConfig({
  plugins: [react(), tako()],
});
```

On the production server build, the plugin writes a wrapper next to the compiled server bundle: `dist/server/tako-entry.mjs`. That wrapper imports your compiled Vite SSR entry, finds the fetch handler, adds Tako's internal status endpoint, and re-exports one default fetch handler for the runtime to launch.

It also matters during `tako dev`. Vite normally prints a localhost URL and calls it a day. Tako waits for a readiness signal on file descriptor 4, then routes local HTTPS traffic through the dev proxy. The plugin binds Vite to loopback, accepts `.test` and `.tako.test` hosts, and reports the bound port back to the parent process so `tako dev` knows the app is actually ready. The [development docs](/docs/development) cover the local proxy flow in more detail.

Now replace the package scripts with the two-build SSR shape Vite documents for production:

```json
{
  "scripts": {
    "dev": "vite dev",
    "build": "npm run build:client && npm run build:server",
    "build:client": "vite build --outDir dist/client",
    "build:server": "vite build --outDir dist/server --ssr src/entry-server.tsx",
    "preview": "vite preview"
  }
}
```

The client build creates `dist/client/index.html` and `/assets/...` files. The server build creates `dist/server/entry-server.js`, and the Tako plugin adds `dist/server/tako-entry.mjs`.

| Output                        | What uses it                                      |
| ----------------------------- | ------------------------------------------------- |
| `dist/client/index.html`      | The server entry reads it as the HTML template    |
| `dist/client/assets/*`        | Tako serves these from deployed `public/assets/*` |
| `dist/server/entry-server.js` | The compiled Vite SSR module                      |
| `dist/server/tako-entry.mjs`  | The entrypoint Tako launches                      |

Run it once:

```bash
npm run build
ls dist/server/tako-entry.mjs
```

If that file exists, Vite and Tako agree on the server entry.

## Step 3 - Tell Tako what to deploy

Install the CLI and initialize the project:

```bash
curl -fsSL https://tako.sh/install.sh | sh
tako init
```

For a custom Vite SSR app, keep the generated config explicit. The plain `vite` preset supplies the Vite dev command, but your SSR entry and client asset directory are project-specific:

```toml
name = "vite-ssr-on-tako"
runtime = "node"
runtime_version = "22.x"
package_manager = "npm"
preset = "vite"
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]

[envs.production]
route = "vite.example.com"
servers = ["prod"]
```

Two lines do most of the SSR work:

| Config                                | Why it matters                                                                 |
| ------------------------------------- | ------------------------------------------------------------------------------ |
| `main = "dist/server/tako-entry.mjs"` | Launch the generated wrapper, not the raw Vite output                          |
| `assets = ["dist/client"]`            | Merge the client build into deployed `public/` so `/assets/*.js` resolves fast |

During deploy, Tako runs the build locally, merges configured asset directories into the artifact's `public/` directory, verifies `main`, packages the result, and uploads it over SFTP. On the server, static requests with file extensions are served directly from `public/` when present; everything else goes to your SSR process. The [Tako config docs](/docs/tako-toml) and [deployment guide](/docs/deployment) have the full field reference.

## Step 4 - Deploy to the VPS

Set up the server once. On the VPS:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

On your laptop, register it:

```bash
tako servers add 203.0.113.10 --name prod
```

Point `vite.example.com` at the VPS IP, then deploy:

```bash
tako deploy
```

Confirm the production prompt and watch the task tree:

```text
Connecting     ✓
Building       ✓
Deploying to prod
  Uploading    ✓
  Preparing    ✓
  Starting     ✓

  https://vite.example.com/
```

Your Vite SSR app is now running as a native Node process behind Pingora, with a real Let's Encrypt certificate. No container runtime is involved.

```d2
direction: right

local: "Local build" {
  client: "vite build\n dist/client" {
    style.fill: "#FFF9F4"
  }
  server: "vite build --ssr\n dist/server" {
    style.fill: "#FFF9F4"
  }
  wrapper: "tako-entry.mjs" {
    style.fill: "#9BC4B6"
  }
  server -> wrapper: "wrap fetch"
}

artifact: ".tar.zst\nartifact" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

vps: "VPS" {
  proxy: "Pingora\nTLS + routing" {
    style.fill: "#E88783"
  }
  public: "public/assets" {
    style.fill: "#FFF9F4"
  }
  node: "Node process\nSSR fetch handler" {
    style.fill: "#9BC4B6"
  }
  proxy -> public: "static files"
  proxy -> node: "HTML requests"
}

local.wrapper -> artifact: "package"
local.client -> artifact: "assets"
artifact -> vps: "SFTP"
```

The request path is simple. `/assets/main-abc123.js` is a static file, so Tako serves it directly from the deployed `public/` directory. `/pricing`, `/dashboard`, or `/` goes to the Node process, which imports `dist/server/tako-entry.mjs`, calls your SSR fetch handler, and returns HTML.

That separation is the whole trick. Vite still does the bundling. React still does the rendering. Tako supplies the deployment boundary around them: native process startup, health checks, static file serving, TLS, and rolling replacement. When you need secrets next, add them with [`tako secrets`](/blog/secrets-without-env-files). When you want to see every CLI shape, the [CLI reference](/docs/cli) is the map.
