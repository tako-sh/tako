---
title: Next.js
description: "Run Next.js with Tako's adapter, standalone build, local HTTPS, and image optimizer, then verify the generated server entry before deployment."
---

Use the `nextjs` preset with the `withTako()` SDK adapter. The repository's example uses Next.js 16.3; the adapter expects the top-level `adapterPath` configuration supported by current Next.js. Keep the app's framework and SDK versions compatible.

## Configure the adapter

Install `tako.sh` with your package manager, then wrap the existing configuration:

```ts next.config.ts
import { withTako } from "tako.sh/nextjs";

export default withTako({
  reactStrictMode: true,
});
```

```toml tako.toml
name = "next-app"
runtime = "node"
preset = "nextjs"

[envs.production]
route = "next.example.com"
servers = ["prod"]
```

Keep a `build` script that runs `next build`. `withTako()` configures standalone output, the Tako adapter, local development origins, and Tako's `next/image` loader. The preset points at `.next/tako-entry.mjs`; do not replace it with `.next/standalone/server.js` or launch a second server yourself.

The adapter stages the Next build and assets for Tako. Existing application routes, React components, and Server Actions stay in Next.js. Use `tako.secrets` only in server-side code; see [Secrets](/secrets/). The image loader uses the default Tako widths; see [Images](/images/).

## Develop and verify

```bash
tako dev --background --json
```

Request the returned HTTPS URL. Test a server-rendered route, a client navigation, a static asset, and an image. Preserve the project's selected bundler settings; the repository example uses `next build --webpack` and `tako dev next dev --webpack`.

Before deploying, run the build script and confirm `.next/tako-entry.mjs` exists. Deploy through [Deployment](/deployment/) and check the same routes on the public hostname. Source example: [examples/javascript/nextjs](https://github.com/tako-sh/tako/tree/master/examples/javascript/nextjs). Upstream: [Next.js adapterPath](https://nextjs.org/docs/app/api-reference/config/next-config-js/adapterPath).
