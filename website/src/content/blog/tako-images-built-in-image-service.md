---
title: "Tako Images: Built-In Image Service for Self-Hosted Apps"
date: "2026-05-11T11:31"
description: "Tako Images turns app-owned images into secure, optimized responses with resizing, smart crops, output formats, and cache rules built in."
image: 653527a88f78
---

Images are sneaky infrastructure.

Your app starts with a few uploads in `public/`, object storage, or a CDN bucket. Then someone wants avatars cropped square, gallery photos capped at a sensible width, optional AVIF for image-heavy pages, private message attachments, and cache headers that do not accidentally make a user's photo reusable in the wrong place. Suddenly "show this image" has become a second platform.

Tako now ships that image service in the app boundary you already own. Keep originals wherever your app keeps images; server-side TypeScript can call `createImageUrl()` from `tako.sh/server`, hand the signed path to the browser, and let `tako-server` verify, resize, encode, and cache the response under `/_tako/image/v1/...`. Storage stays yours. Transformation and policy move into Tako. No separate optimizer service. No query-string soup.

## One helper signs the contract

The smallest version is deliberately boring:

```ts
import { createImageUrl } from "tako.sh/server";

const photo = createImageUrl("/photos/p_123.jpg");
```

That signs a private WebP URL with maximum width `1200`, quality `75`, a 7-day expiration, and 7-day browser-only caching. The return value is a path on your own app:

```txt
/_tako/image/v1/<payload>.<signature>
```

The browser never sees the signing secret. Your server code receives the app-scoped image secret through Tako's fd 3 bootstrap envelope, the SDK signs a compact payload, and the proxy verifies the signature before it fetches or decodes any image bytes. If someone tampers with width, source, quality, expiration, format, or cache policy, the signature stops matching.

That is the first design choice: URLs are private by default. Use the default for user-specific images, message attachments, account photos, and anything where a shared cache should not keep a copy.

```ts
const avatar = createImageUrl(`/avatars/${user.id}.png`, {
  width: 256,
  height: 256,
  crop: "smart",
});

const messagePhoto = createImageUrl(`/messages/${message.id}/photo.jpg`, {
  width: 1200,
  height: 800,
  fit: "cover",
  crop: "smart",
  browserCacheMaxAgeSeconds: 2_592_000,
});
```

Private responses use `Cache-Control: private`, so the browser can reuse the result, but shared caches must not. If you do have a non-user-specific asset that should be stable and publicly cacheable, say that explicitly:

```ts
const hero = createImageUrl("/assets/home-hero.jpg", {
  width: 1200,
  quality: 80,
  public: true,
});
```

Public image URLs have no expiration and use long immutable public cache headers. Tako makes that an option, not the default, because "this can be shared forever" is a real product decision.

## Resize, crop, and format without a side service

Tako's optimizer is intentionally narrow. It accepts local paths or remote `http`/`https` sources, rejects private and local remote hosts, and transforms JPEG, PNG, WebP, and AVIF sources by file signature. It emits WebP by default, or AVIF when you opt into the smaller, slower-to-encode format:

```ts
const avif = createImageUrl("/avatars/u_123.png", {
  width: 256,
  format: "avif",
});
```

You do not pass `format: "webp"` because WebP is the default. Omitting `format` keeps the payload smaller and leaves the default obvious.

Width-only requests preserve aspect ratio and never upscale. If the original image is `800px` wide and you request `1200`, the output stays `800px`. Fixed boxes require both `width` and `height`, then choose `cover` or `contain`:

```ts
const square = createImageUrl("/uploads/profile.jpg", {
  width: 384,
  height: 384,
  fit: "cover",
  crop: "smart",
});

const product = createImageUrl("/catalog/backpack.png", {
  width: 640,
  height: 640,
  fit: "contain",
});
```

`cover` fills as much of the box as possible, then crops overflow. `crop: "smart"` uses libvips attention cropping, which is useful for thumbnails where the interesting part should survive. `contain` fits inside the box without cropping and rejects `crop`, because there is nothing to crop.

The useful shape is easier to scan as a table:

| Need                   | Options                             | Result                                                   |
| ---------------------- | ----------------------------------- | -------------------------------------------------------- |
| Regular private photo  | omitted or `{ width }`              | WebP, max width `1200` by default, private browser cache |
| Square avatar          | `{ width, height, crop: "smart" }`  | Cover resize with attention crop                         |
| Product image in a box | `{ width, height, fit: "contain" }` | Fits inside the box without cropping or upscaling        |
| Smaller AVIF variant   | `{ format: "avif" }`                | AVIF output when the tradeoff is worth it                |
| Public marketing asset | `{ public: true }`                  | Stable public URL with immutable cache headers           |

The server side uses libvips for resize, crop, and encode work, and strips metadata from transformed output. Server installs include the host libvips runtime, so this is part of the same [`tako-server`](/docs/deployment) surface that already handles routing, TLS, deploys, and static assets.

```d2
direction: right

app: "TypeScript server code" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
browser: Browser {
  style.fill: "#9BC4B6"
}
proxy: "Tako proxy\n/_tako/image/v1" {
  style.fill: "#E88783"
}
source: "public/ or app backend" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
vips: "libvips transform" {
  style.fill: "#9BC4B6"
}

app -> browser: "img src = signed path"
browser -> proxy: "GET signed image URL"
proxy -> proxy: "verify payload signature"
proxy -> source: "fetch original image"
source -> vips: "JPEG / PNG / WebP / AVIF"
vips -> browser: "WebP or AVIF + cache headers"
```

## Why this belongs in Tako

Image optimization sits in the same awkward place as secrets, WebSocket channels, and workflows: too app-specific to be pure infrastructure, too operational to copy into every route handler. The app knows which source image to show, whether it is private, and which crop makes sense. The platform should own signature verification, SSRF protection, byte limits, resize math, cache headers, and the actual image transform.

That split is why `createImageUrl()` is a small server-only SDK helper instead of a framework component. You can call it from a Hono handler, a TanStack Start server function, a Next.js server component, or plain fetch-handler code. The browser only gets a path. The proxy does the heavy work after the request matches your route, alongside the other reserved `/_tako/*` endpoints described in [How Tako Works](/docs/how-tako-works).

It also keeps deployment simple. If you can deploy the app with [`tako deploy`](/docs/cli), the optimizer comes with it. Sources in `public/` are served locally when present; other local paths can be fetched from the matched app backend. Remote image sources are allowed only through the signed URL contract, with unsupported schemes, userinfo, fragments, local/private hosts, local/private DNS results, recursive optimizer URLs, and redirects rejected before transform work happens.

That is the difference from handing raw storage URLs straight to the browser. A CDN can store bytes. Tako can enforce your app's own policy because the URL was minted by your app, signed with your app's secret, and served by your app's platform boundary.

## The shape we wanted

The image optimizer is not trying to be a giant media pipeline. It is the 80% path most self-hosted apps need:

| Concern  | Tako behavior                                                           |
| -------- | ----------------------------------------------------------------------- |
| Privacy  | Signed URLs are private by default and expire by default                |
| Formats  | WebP by default, AVIF on request                                        |
| Resizing | Fixed allowed dimensions, no upscaling                                  |
| Cropping | Center or libvips smart crop for cover thumbnails                       |
| Caching  | Browser-only private cache by default, explicit immutable public cache  |
| Sources  | Local app images or remote HTTP(S), with private/local targets rejected |

That makes images feel like the rest of Tako: your code declares intent, and the platform takes the sharp edges. Read the full config and routing model in [`tako.toml`](/docs/tako-toml) and [deployment docs](/docs/deployment), or jump into the [Tako repo](https://github.com/lilienblum/tako) if you want to see the signed payload contract in code.
