---
title: "Image Optimization for TanStack Start Apps on a VPS"
date: "2026-05-16T13:26"
description: "Optimize TanStack Start images on a VPS with Tako's public image endpoint, responsive srcsets, remote allowlists, and WebP output."
image: 5b5b0f21ec5b
---

TanStack Start gives React apps a clean full-stack shape: routes, SSR, server functions, and a normal Vite build. Images are the part that still tries to become infrastructure.

A hero image looks harmless in `public/images/hero.jpg`. Then the homepage needs modern output, cards need smaller variants, remote CMS images need an allowlist, and you would rather not make the browser download a 3000px original into a 640px slot. Hosted platforms usually hide that behind an image component. If you are running TanStack Start on your own VPS, you need the same boringly useful piece close to the app.

That is what Tako's public image optimizer is for. The app renders plain `<img>` markup, the SDK builds CDN-friendly URLs, and `tako-server` does the resize and encode work behind your route. If you already followed the [TanStack Start deploy guide](/blog/deploy-tanstack-start-to-a-vps-in-five-minutes/), images use the same platform boundary as routing, TLS, static assets, and [zero-downtime deploys](/blog/zero-downtime-deploys-without-a-container-in-sight/).

## The TanStack Start Shape

TanStack Start does not need a special image component for Tako. Use normal React components and generate the `src`, `srcset`, and `sizes` values with `imageSrcSet` from `tako.sh`.

```tsx
// app/components/HeroImage.tsx
import { imageSrcSet } from "tako.sh";

export function HeroImage() {
  const hero = imageSrcSet("/images/product-hero.jpg", {
    layout: "constrained",
    width: 1200,
    quality: 75,
  });

  return (
    <img
      src={hero.src}
      srcSet={hero.srcSet}
      sizes={hero.sizes}
      width={1200}
      height={800}
      alt="Product dashboard showing recent orders"
      loading="eager"
      decoding="async"
    />
  );
}
```

The returned URLs point at Tako's reserved public endpoint:

```text
/_tako/image?src=/images/product-hero.jpg&w=1200
```

For a constrained 1200px image, `imageSrcSet` creates candidate widths from Tako's configured width list and derives a useful `sizes` value:

| Option                     | Meaning                                                         |
| -------------------------- | --------------------------------------------------------------- |
| `layout: "constrained"`    | The image can shrink with the viewport but stops at `width`.    |
| `width: 1200`              | The fallback `src` width and maximum rendered width.            |
| `quality: 75`              | Tako's default quality, included here so the choice is visible. |
| Generated `sizes`          | `(min-width: 1200px) 1200px, 100vw`                             |
| Generated candidate widths | Default allowed widths up to the responsive maximum.            |

Use `imageUrl` when you need exactly one URL:

```tsx
import { imageUrl } from "tako.sh";

const avatar = imageUrl("/avatars/u_123.png", {
  width: 640,
});
```

Use `imageSrcSet` for layout images, gallery images, product photos, and anything that may render at multiple viewport widths. The markup stays framework-neutral, which is useful in TanStack Start because the same component can live in a route, a shared React component, or server-rendered page content without introducing a platform-specific image abstraction.

```d2
direction: right

route: "TanStack Start route\nor React component" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
html: "SSR HTML\n<img srcset=...>" {
  style.fill: "#9BC4B6"
}
proxy: "tako-server\n/_tako/image" {
  style.fill: "#E88783"
}
source: "public/ image\nor allowlisted remote URL" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
vips: "libvips\nresize + encode" {
  style.fill: "#9BC4B6"
}

route -> html: "imageSrcSet()"
html -> proxy: "browser requests variant"
proxy -> source: "fetch original"
source -> vips: "JPEG / PNG / WebP / AVIF"
vips -> html: "WebP by default\nor AVIF opt-in"
```

## Configure The Guardrails

Local public images work by default. If your TanStack Start app has `public/images/product-hero.jpg`, you can use `/images/product-hero.jpg` as the source path. During deploy, the `tanstack-start` preset already knows how to run the build and ship the client assets; the broader routing model is covered in [How Tako Works](/docs/how-tako-works/) and the preset fields are listed in [Framework Presets](/docs/presets/).

Remote images are different. Tako denies them until you allow the origin in `tako.toml`:

```toml
runtime = "bun"
preset = "tanstack-start"

[images]
remote_patterns = ["https://cdn.example.com/uploads/**"]
sizes = [320, 640, 960, 1200, 1920]
qualities = [75]
formats = ["webp"]
```

Those fields are intentionally narrow:

| Field             | Default                       | Why it exists                                              |
| ----------------- | ----------------------------- | ---------------------------------------------------------- |
| `local_patterns`  | `["/**"]`                     | Restrict which local public paths the optimizer can read.  |
| `remote_patterns` | `[]`                          | Allow only the remote image origins your app expects.      |
| `sizes`           | `[320, 640, 960, 1200, 1920]` | Keep generated variants finite and cacheable.              |
| `qualities`       | `[75]`                        | Prevent arbitrary quality values from becoming cache keys. |
| `formats`         | `["webp"]`                    | Keep output formats predictable; add AVIF when needed.     |

Patterns are glob-like URL strings, not regular expressions. `*` matches one path segment, `**` matches the rest, and a remote host can use a leading wildcard such as `https://*.example.com/uploads/**`. A pattern without a protocol allows both `http` and `https`, though most production image origins should be explicit.

The endpoint fails closed. `src` and `w` are required, `q` and `f` are optional, and duplicate or unknown query params are rejected. Width, quality, and format must match the configured lists. Remote sources must be `http` or `https`, cannot contain userinfo or fragments, cannot recursively call the image optimizer, and cannot resolve to private or local network targets.

That sounds strict because it is. An image optimizer is a fetcher, decoder, CPU user, and cache-key generator. The useful version is not "let the browser ask for anything." The useful version is "let the app choose from a small set of variants that are safe to cache forever."

## Deploy It Like The Rest Of The App

There is no extra image server to run for TanStack Start. The same `tako-server` that terminates TLS and routes requests owns the image endpoint after a request matches your app route. The server installer also installs libvips for image optimization, and the [deployment docs](/docs/deployment/) cover the server setup path.

```bash
tako init
tako deploy
```

For TanStack Start, `tako init` detects the app and offers the `tanstack-start` preset. That preset points Tako at the generated server entry and client assets, so your SSR handler, static files, and image optimizer all land in one deploy artifact. The [Tako config reference](/docs/tako-toml/) has the full `[images]` field list when you want tighter local paths or remote sources.

In production, public optimized responses use long immutable cache headers. Transforms preserve aspect ratio, do not upscale, apply EXIF orientation before encoding, strip source metadata, and emit WebP by default. Public AVIF variants are available for still images when you add AVIF to `[images].formats` and request it explicitly, or put AVIF first when you want `Accept` negotiation to choose it. Sources are accepted by file signature for JPEG, PNG, GIF, WebP, and AVIF rather than trusting `Content-Type` alone; animated GIF and WebP sources keep animation for optimized resize and crop URLs, and animated AVIF requests fall back to WebP.

Here is the practical decision table:

| Image need                 | Use this                                                                    |
| -------------------------- | --------------------------------------------------------------------------- |
| Above-the-fold hero        | `imageSrcSet(..., { layout: "constrained", width: 1200 })`                  |
| Full-bleed banner          | `imageSrcSet(..., { layout: "full-width", width: 1920 })`                   |
| Fixed avatar               | `imageUrl(..., { width: 640 })`                                             |
| CMS or bucket image        | Add `remote_patterns`, then use the remote `https://...` URL.               |
| Public object storage file | Add `public_base_url`, then use `createImageSrcSet(..., { public: true })`. |

The important thing is where the contract lives. Your TanStack Start app decides which original image belongs on the page. `tako.sh` turns that decision into a small set of optimizer URLs. `tako-server` enforces the allowlists, does the resize work, and serves cacheable WebP or configured AVIF from the same route that already serves the app.

That keeps the happy path small: build a TanStack Start app, deploy it to your VPS, and render responsive optimized images without adding a media service, a framework-specific image component, or a second proxy. The app stays React. The image pipeline becomes part of the platform layer Tako is already running for you. Start with the [framework guide](/docs/framework-guides/), then wire images through `imageSrcSet` when the first oversized hero image shows up.
