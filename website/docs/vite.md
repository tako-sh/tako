---
title: Vite
description: "Use Tako with Vite development servers and fetch-compatible SSR output, configure the actual build entry and assets, and verify readiness."
---

The `vite` preset provides a development command. It does not choose a production server format or make every Vite-based framework compatible automatically. Use the dedicated [TanStack Start](/tanstack-start/) guide when applicable.

## Connect a dev server

Add Tako alongside your existing Vite plugins:

```ts vite.config.ts
import { defineConfig } from "vite";
import { tako } from "tako.sh/vite";

export default defineConfig({
  plugins: [tako()], // Retain the app's framework plugins here too.
});
```

The plugin allows Tako hostnames, initializes SDK bootstrap, and reports the listening port over fd 4. Start through `tako dev --background --json`; opening a raw Vite port bypasses Tako routing and HTTPS.

## Configure production output

For an app whose SSR build exports a fetch handler:

```toml tako.toml
name = "vite-app"
runtime = "bun"
preset = "vite"
main = "dist/server/tako-entry.mjs"
assets = ["dist/client"]

[build]
run = "bun run build"

[envs.production]
route = "vite.example.com"
servers = ["prod"]
```

The `main` and `assets` paths must match this application's build. Vite supports separate client and SSR builds, but its SSR entry is not necessarily an HTTP fetch handler: it may export a rendering function instead. Tako's wrapper requires a default fetch function, default object with `fetch`, or named `fetch` export.

The plugin emits `tako-entry.mjs` beside a selected entry chunk. A client-only build is not a production HTTP server. For a custom fetch server, see [Fetch handlers](/fetch-handler/); for an unsupported framework output, supply a compatible adapter before deploying.

## Verify

Run the build, inspect the emitted server exports and paths, then verify SSR, hydration, assets, and deep routes through Tako. If the build cannot select one server entry or runtime loading reports an invalid handler, fix the server build rather than pointing `main` at an arbitrary client chunk. Upstream: [Vite SSR](https://vite.dev/guide/ssr).
