---
title: "Self-Hosted Next.js Image Optimization on a VPS"
date: "2026-05-20T14:19"
description: "Use next/image with Tako's self-hosted optimizer on a VPS: custom loader wiring, remote allowlists, WebP output, caches, and fallbacks."
image: c282f5235ed5
---

Next.js made images feel like a component. Drop in `<Image>`, give it a source, and let the platform worry about width variants, modern formats, cache headers, and remote source rules.

That works beautifully when the platform is Vercel. It also works self-hosted with `next start`, because Next optimizes images at runtime. But when you deploy a Next.js app to your own VPS with Tako, there is a better place for that work to live: the same server boundary that already owns routing, TLS, static files, logs, and [zero-downtime deploys](/docs/deployment).

Tako lets you keep `next/image`. The handoff happens underneath it. `withTako()` configures Next's custom image loader so generated image URLs point at `/_tako/image`, then `tako-server` validates the request, loads the source, resizes with libvips, caches the result, and sends WebP by default.

## The `next/image` handoff

Next's image component has two extension points that matter here. A custom loader receives `src`, `width`, and optional `quality`, then returns the URL the browser should request. A `loaderFile` in `next.config.js` applies that loader globally, so every `<Image>` component can use the same image service.

Tako wraps that config for you:

```ts
// next.config.ts
import { withTako } from "tako.sh/nextjs";

export default withTako({
  images: {
    remotePatterns: [
      {
        protocol: "https",
        hostname: "cdn.example.com",
        pathname: "/uploads/**",
      },
    ],
  },
});
```

Under the hood, `withTako()` preserves your config, then applies the pieces Tako needs:

| Next config field    | Value Tako sets                 | Why it matters                                              |
| -------------------- | ------------------------------- | ----------------------------------------------------------- |
| `output`             | `"standalone"`                  | Gives Tako a deployable server output.                      |
| `adapterPath`        | the `tako.sh/nextjs` adapter    | Lets Next write `.next/tako-entry.mjs` after build.         |
| `allowedDevOrigins`  | adds `*.test` and `*.tako.test` | Lets `tako dev` proxy requests through local HTTPS hosts.   |
| `images.loader`      | `"custom"`                      | Tells `next/image` not to use Next's default optimizer URL. |
| `images.loaderFile`  | Tako's packaged loader          | Converts image props into `/_tako/image` URLs.              |
| `images.deviceSizes` | `[320, 640, 960, 1200, 1920]`   | Aligns Next's generated widths with Tako's defaults.        |
| `images.imageSizes`  | `[]`                            | Keeps the generated variant set small and predictable.      |

That means your component still looks like ordinary Next.js:

```tsx
import Image from "next/image";

export function Hero() {
  return (
    <Image
      src="/images/product-hero.jpg"
      alt="Product dashboard"
      width={1200}
      height={800}
      priority
    />
  );
}
```

When Next renders the page, Tako's loader turns the image into a public optimizer URL:

```text
/_tako/image?src=%2Fimages%2Fproduct-hero.jpg&w=1200
```

If the component asks for a different quality, that becomes `q=...`. If no format is specified, the server negotiates from the browser's `Accept` header against the app's configured format list. With the default config, the output is WebP.

## Two allowlists, two jobs

Remote image rules exist in two places because they protect different boundaries.

Next's [`remotePatterns`](https://nextjs.org/docs/app/api-reference/components/image) setting keeps the component honest. If someone passes a remote `src` that does not match the configured protocol, hostname, port, path, and search constraints, Next rejects it.

Tako's `[images]` config is the runtime boundary. It controls what `tako-server` is allowed to fetch and transform after a real browser request reaches your VPS. Local public paths are allowed by default. Remote URLs are denied until you allow them in [`tako.toml`](/docs/tako-toml):

```toml
runtime = "node"
preset = "nextjs"

[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
formats = ["webp"]
```

For a typical self-hosted Next app, keep both sides aligned:

| Need                      | Configure in Next           | Configure in Tako                                                       |
| ------------------------- | --------------------------- | ----------------------------------------------------------------------- |
| Local images in `public/` | nothing special             | local paths work by default                                             |
| Remote CMS images         | `images.remotePatterns`     | `[images].remote_patterns`                                              |
| Default responsive widths | `withTako()` sets them      | defaults are already the same                                           |
| WebP output               | no component change         | default `formats = ["webp"]`                                            |
| AVIF output               | usually no component change | add `formats = ["avif", "webp"]` if you want negotiation to prefer AVIF |

The patterns are intentionally strict. `*` matches one path segment, `**` matches the rest of a path, and remote hosts can use a leading wildcard such as `https://*.example.com/uploads/**`. Remote sources must be `http` or `https`, cannot include userinfo or fragments, cannot point back at the image optimizer, and cannot resolve to private or local network targets.

That last part matters. An image optimizer is a server-side fetcher and a CPU user. The useful version is not "let any browser request any URL at any size." The useful version is "let the app publish a small, finite set of image variants that are safe to fetch, transform, and cache."

## What the VPS does

After a request matches your app route, Tako reserves `/_tako/*` for platform endpoints. Channels live there, storage object URLs live there, and public optimized images live at `/_tako/image`. The route is part of the same [app serving model](/docs/how-tako-works) as your Next process.

```d2
direction: right

component: "Next <Image>\nwidth candidates" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
loader: "Tako loader\n/_tako/image URL" {
  style.fill: "#9BC4B6"
}
server: "tako-server\nallowlist + fetch" {
  style.fill: "#E88783"
}
worker: "libvips worker\nresize + encode" {
  style.fill: "#9BC4B6"
}
cache: "transform cache\nWebP or AVIF response" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

component -> loader: "src, width, quality"
loader -> server: "browser requests URL"
server -> worker: "validated original"
worker -> cache: "store variant"
cache -> component: "serve hit or new transform"
```

The request has to pass validation before source bytes are loaded. `src` and `w` are required. `q` and `f` are optional. Duplicate or unknown query params are rejected. Width, quality, and format must match the lists in `[images]`.

Then the server loads the original. Local paths resolve from the deployed `public/` directory first, then from the matched app backend. Remote sources use a guarded HTTP client with no proxy, no redirects, connection and request timeouts, and DNS checks that reject private or local addresses.

The performance path has two caches:

| Cache           | Scope                                      | Purpose                                                       |
| --------------- | ------------------------------------------ | ------------------------------------------------------------- |
| Source cache    | in memory, 10 seconds, 64 MiB, 256 entries | Reuses the same original when a page asks for several widths. |
| Transform cache | local disk under `/tmp/tako-image-cache`   | Reuses finished variants across requests.                     |

Transform cache keys include the app name, release root, source bytes, output format, width, optional height/fit/crop, and quality. That means a new deploy or changed source file naturally produces a new cache key. The cache is best effort and local to each server. Tako prunes entries older than 30 days, then keeps the cache within a filesystem-based cap: 5% of the filesystem, clamped between 1 GiB and 4 GiB.

The actual resize and encode work runs in an isolated child process. That is not an aesthetic choice. Image codecs are native code, and native code deserves a process boundary. Tako limits concurrent transforms, queues a bounded number of misses, and times out work that does not finish. Cache hits and duplicate in-flight misses skip the worker queue entirely.

If transform work fails after a verified image source was already loaded, Tako can serve the original image bytes as a fallback when the source response has an `image/*` content type. That fallback is deliberately marked `Cache-Control: private, no-store`, so a transient resize failure does not become the permanent public optimized response. Validation failures, source-size failures, decoded-image safety failures, and a full transform queue do not fall back.

In practice, this keeps the failure mode narrow. A bad remote URL fails fast, an oversized source never reaches the expensive path, and a temporary encoder problem can still let a real browser see the original image instead of a broken page.

## Deploy it like a Next app

There is no separate image service to boot. Install the SDK, wrap the config, add any remote allowlists, then deploy the app through the normal [Tako CLI](/docs/cli) flow:

```bash
bun add tako.sh
tako init
tako deploy
```

For Next.js, the `nextjs` preset uses `.next/tako-entry.mjs` as the entrypoint. The adapter writes that file after `next build`. If Next emits `.next/standalone/server.js`, Tako stages the standalone server with `public/` and `.next/static/` copied into the right places. If standalone output is missing, the wrapper falls back to `next start` against the built `.next/` directory.

The image path stays boring from the developer side:

1. Use `<Image>` in your Next app.
2. Use `withTako()` in `next.config.ts`.
3. Put local images in `public/`, or add remote origins to both Next and Tako.
4. Deploy to the VPS.

The interesting part is where the operational work moved. Your React code chooses the image. Next chooses responsive widths. `tako.sh` converts that into a platform URL. `tako-server` enforces the allowlist, transforms the bytes, caches the variant, and logs failures where `tako logs` can show them.

That is the kind of infrastructure Tako is trying to make feel ordinary on your own hardware. Not a second image vendor, not a custom Next route, not a hand-rolled sharp endpoint. Just your Next.js app, a VPS, and the platform layer that is already serving the rest of the request.
