---
layout: ../../layouts/DocsLayout.astro
title: Framework guides for Next.js, Astro, Nuxt, SvelteKit, and more - Tako Docs
heading: Framework Guides
current: framework-guides
description: "Framework-specific Tako guides for Next.js, Astro, SvelteKit, Nuxt, TanStack Start, and other apps using fetch handlers or Vite."
---

# Framework Guides

## Pick a preset first

If your framework matches a Tako preset, set `preset` in `tako.toml` and Tako fills in the entrypoint, asset directories, and dev command for you. Always prefer this over wiring `main` by hand.

| Framework      | `runtime`      | `preset`         |
| -------------- | -------------- | ---------------- |
| TanStack Start | `bun` / `node` | `tanstack-start` |
| Next.js        | `bun` / `node` | `nextjs`         |
| Other Vite app | `bun` / `node` | `vite`           |

See [Presets](/docs/presets) for what each preset sets.

## TanStack Start (and other Vite SSR frameworks)

Install the Tako Vite plugin so the build emits the SSR wrapper:

```ts
import { defineConfig } from "vite";
import { tako } from "tako.sh/vite";

export default defineConfig({
  plugins: [tako()],
});
```

Then in `tako.toml`:

```toml
runtime = "bun"
preset = "tanstack-start"
```

The `tanstack-start` preset sets `main = "dist/server/tako-entry.mjs"`, `assets = ["dist/client"]`, and `dev = ["vite", "dev"]` — no manual `main` needed. Use the `vite` preset for non-SSR Vite apps.

## Next.js

Wrap your Next config with the Tako helper:

```ts
import { withTako } from "tako.sh/nextjs";

export default withTako({});
```

This enables Next.js standalone output, installs the Tako adapter, adds `*.test` and `*.tako.test` to `allowedDevOrigins` so `next dev` accepts requests from Tako's dev hostnames, and generates `.next/tako-entry.mjs` for deploys. If Next emits standalone output, Tako uses it; otherwise the wrapper falls back to `next start`.

Then in `tako.toml`:

```toml
runtime = "bun"
preset = "nextjs"
```

## Fallback: fetch handler (no preset)

For frameworks without a preset, export a standard fetch handler from your build output and point `main` at it:

```ts
export default function fetch(request: Request, env: Record<string, string>) {
  return new Response("Hello from Tako");
}
```

```toml
runtime = "bun"
main = "dist/server/index.js"
```

Tako automatically runs your app with the correct runtime (Bun or Node.js) based on your project configuration.
