---
title: TanStack Start
description: "Connect a React TanStack Start app to Tako's Vite adapter, build the fetch-compatible server entry, and verify SSR, assets, and local HTTPS."
---

Use the `tanstack-start` preset for a React TanStack Start application. Add Tako to the existing Vite configuration without removing the framework's plugins.

## Configure Vite and Tako

```ts vite.config.ts
import { defineConfig } from "vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import viteReact from "@vitejs/plugin-react";
import { tako } from "tako.sh/vite";

export default defineConfig({
  plugins: [tanstackStart(), viteReact(), tako()],
});
```

```toml tako.toml
name = "start-app"
runtime = "bun"
preset = "tanstack-start"

[envs.production]
route = "start.example.com"
servers = ["prod"]
```

Keep the React plugin after `tanstackStart()`. The Tako preset supplies `main = "dist/server/tako-entry.mjs"`, `assets = ["dist/client"]`, and the Vite dev command. Keep a `build` script that runs `vite build`; no manual server entry override is needed for this output layout.

The Tako plugin wraps the compiled fetch-compatible server entry, preserves internal status/auth endpoints, externalizes the server SDK, and reports dev readiness. It initializes the server runtime for [Secrets](/secrets/), [Stores](/stores/), and [Workflows](/workflows/). Keep server SDK access out of client components.

## Develop and verify

```bash
tako dev --background --json
```

Use the returned HTTPS URL. Check an SSR response, client-side navigation, static assets, and any server functions the app uses. Build before deployment and confirm both `dist/server/tako-entry.mjs` and `dist/client` exist. For a path-mounted app, align Vite's base with the configured route and test nested URLs.

Deploy using [Deployment](/deployment/). Source example: [examples/javascript/tanstack-start](https://github.com/tako-sh/tako/tree/master/examples/javascript/tanstack-start). Upstream: [TanStack Start configuration](https://tanstack.com/start/latest/docs/framework/react/build-from-scratch).
