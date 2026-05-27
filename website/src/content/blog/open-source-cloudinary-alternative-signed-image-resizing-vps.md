---
title: "The Open Source Cloudinary Alternative for Signed Image Resizing on a VPS"
seoTitle: "Open Source Cloudinary Alternative for VPS Images"
date: "2026-05-13T04:39"
description: "Compare Cloudinary-style image transforms with Tako's self-hosted signed URLs, WebP/AVIF output, and app-owned cache policy."
image: 0e292c22eac5
---

Cloudinary is great because it made image infrastructure feel like a URL.

Upload an original, ask for the shape you need, and the image pipeline takes care of resize, crop, format, delivery, and cache behavior. [ImageKit](https://imagekit.io/docs/image-transformation) has the same general appeal: put transformation intent in the delivery path, keep your app code small, and stop writing one-off resize handlers for every avatar, gallery, and product card.

That is the right shape. The question is where it should live.

For a lot of apps, a full hosted media platform is the right answer. If media is its own product surface, use the thing built for that. Tako is not trying to become that. Tako's image service is smaller on purpose: signed image resizing, safe source fetching, WebP/AVIF output, and cache policy inside the same self-hosted platform that already runs your app on a VPS.

## The Difference Is Ownership

Cloudinary and ImageKit are media platforms. You integrate your app with their delivery layer, and their URL format becomes the public contract for media. Cloudinary documents [delivery URL signatures](https://cloudinary.com/documentation/delivery_url_signatures) for protecting transformed delivery URLs. ImageKit documents [basic media delivery security](https://imagekit.io/docs/media-delivery-basic-security) and URL-based transformation parameters. Both are mature, useful systems.

Tako starts from a different assumption: the image is part of the app.

Your server code calls `createImageUrl()` from `tako.sh/server`, gets back a path under your own app, and sends that to the browser:

```ts
import { createImageUrl } from "tako.sh/server";

const avatar = createImageUrl(`/avatars/${user.id}.jpg`, {
  width: 256,
  height: 256,
  crop: "smart",
});
```

The returned URL is not a third-party domain and not a free-form query string. It is a signed path under Tako's reserved image endpoint:

```txt
/_tako/image/v1/<payload>.<signature>
```

The app server receives an app-scoped image secret through Tako's runtime bootstrap. The browser never sees it. When the browser requests the path, `tako-server` verifies the signature before it fetches or decodes the source image. If someone changes the source path, width, height, crop mode, output format, expiration, quality, or cache setting, the signature stops matching.

Hosted media platforms make image delivery an external platform contract. Tako makes it an app-owned contract enforced by the proxy that already handles routing, TLS, channels, static assets, and deploys. The broader routing model is documented in [How Tako Works](/docs/how-tako-works/), and the deploy surface is in [Deployment](/docs/deployment/).

| Concern              | Cloudinary / ImageKit style                     | Tako style                                     |
| -------------------- | ----------------------------------------------- | ---------------------------------------------- |
| Where transforms run | Hosted media platform                           | Your `tako-server`                             |
| URL host             | Media service domain or configured media host   | Your app route                                 |
| Signing secret       | Media platform integration secret               | App-scoped Tako image secret                   |
| Storage model        | Platform-managed or configured origin storage   | App-owned local paths or signed remote sources |
| Best fit             | Full media pipeline and global delivery product | App-owned images on self-hosted apps           |

None of that makes one side universally better. It just changes the tradeoff. If images are one infrastructure need inside a self-hosted app, Tako keeps the moving parts close to the app that knows the policy.

## Signed Resizing Without Query-String Soup

The easiest way to make image resizing dangerous is to let the browser invent transformations.

An open-ended URL like "source plus arbitrary width plus arbitrary format plus arbitrary crop" looks convenient until someone discovers they can ask your server to generate thousands of variants, fetch internal URLs, or force expensive work on huge source files. Hosted platforms have their own controls for this. Tako's answer is narrower: the SDK only signs the options Tako supports, and the proxy rejects anything outside that contract.

The default call is private and conservative:

```ts
const photo = createImageUrl("/uploads/p_123.jpg");
```

That produces a private WebP URL with maximum width `1200`, quality `75`, a 7-day expiration, and browser-only private caching. For public marketing assets, you say so explicitly:

```ts
const hero = createImageUrl("/assets/home-hero.jpg", {
  width: 1200,
  quality: 80,
  public: true,
});
```

Public image URLs are stable, have no expiration, and use long immutable public cache headers. Private image URLs expire and use private browser cache headers. That difference is not buried in CDN config. It is a choice in the app code that minted the URL.

The transform surface is intentionally small:

| Need                     | Tako option                                   | Result                                 |
| ------------------------ | --------------------------------------------- | -------------------------------------- |
| Default responsive image | omitted or `{ width: 1200 }`                  | WebP, no upscaling, private by default |
| Square avatar            | `{ width: 256, height: 256, crop: "smart" }`  | Cover resize with attention crop       |
| Product image box        | `{ width: 640, height: 640, fit: "contain" }` | Fit inside the box without cropping    |
| Smaller AVIF variant     | `{ format: "avif" }`                          | AVIF when the tradeoff is worth it     |
| Public evergreen asset   | `{ public: true }`                            | Immutable public cache policy          |

The limits are part of the product. Widths and heights come from a fixed set. Quality is `1..100`. Output is WebP by default or AVIF when requested for still images. Source images are JPEG, PNG, GIF, WebP, or AVIF by file signature, and animated GIF/WebP sources keep animation for optimized resize and crop URLs. Animated AVIF requests fall back to WebP so motion is preserved. Transform work uses libvips, strips metadata, respects EXIF orientation, and never upscales.

That is less flexible than a general media platform. It is also much harder to misuse.

```d2
direction: right

app: "Your app server" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
browser: Browser {
  style.fill: "#9BC4B6"
}
proxy: "tako-server\n/_tako/image/v1" {
  style.fill: "#E88783"
}
source: "App image source\npublic/ or backend" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
vips: "libvips\nWebP / AVIF" {
  style.fill: "#9BC4B6"
}

app -> browser: "return signed image path"
browser -> proxy: "GET image path"
proxy -> proxy: "verify signature and expiration"
proxy -> source: "fetch original safely"
source -> vips: "decode and transform"
vips -> browser: "optimized bytes + cache headers"
```

## The Security Boundary Is The App Boundary

The quiet feature in Tako Images is not AVIF. It is where the security decision happens.

Your app decides which image source should be visible, which size is allowed, whether the result is private, and how long the browser may cache it. Tako signs that decision. The proxy verifies it before any source bytes are fetched.

Remote sources are allowed, but not casually. Tako rejects unsupported schemes, userinfo, fragments, recursive image optimizer URLs, private and local hosts, private and local IPs, and private or local DNS results. Redirects are not followed for remote image fetches. Source byte limits and decoded image limits protect the transform path. Failed optimizer responses use `Cache-Control: private, no-store`.

That matters for self-hosted apps because the image optimizer sits next to your app. If a VPS app accepts an arbitrary remote image URL and then fetches it from the server side, it needs SSRF guardrails. Tako bakes those guardrails into the platform.

The same pattern shows up elsewhere in Tako. Secrets are delivered through the runtime bootstrap, durable channels live under reserved `/_tako/*` routes, and workflows talk to the server over an internal socket. The app declares intent; the platform enforces the dangerous edges. The SDK shape is covered in the [framework guides](/docs/framework-guides/), and the config surface lives in [`tako.toml`](/docs/tako-toml/).

## When To Use Which

| You need                                                                                                           | Pick                   |
| ------------------------------------------------------------------------------------------------------------------ | ---------------------- |
| Digital asset management, upload workflows, video, rich transformation catalogs, and global managed media delivery | Cloudinary or ImageKit |
| A simple hosted image CDN for a team that does not want to operate media transforms                                | Cloudinary or ImageKit |
| Signed thumbnails, private user images, avatars, product photos, and local app assets on a self-hosted VPS app     | Tako                   |
| App-owned cache policy where private is the default and public is explicit                                         | Tako                   |
| One deploy surface for app code, routing, TLS, secrets, workflows, channels, static files, and image resizing      | Tako                   |

Tako is the open-source Cloudinary alternative only in the slice that many app developers need first: signed image resizing on the server they already own. It is trying to make the common case boring enough that you do not need to bolt on another service just to crop an avatar.

That is the broader Tako bet. A deployment platform should not stop at "your process is running." Real apps need HTTPS, routes, secrets, logs, workflows, channels, and images. You can assemble those from separate services, and sometimes you should. But when the primitive belongs to the app, Tako tries to keep it inside the same app-shaped boundary.

Read the full image behavior in [How Tako Works](/docs/how-tako-works/), deploy it through the normal [`tako deploy`](/docs/cli/) flow, or browse the implementation in the [Tako repo](https://github.com/tako-sh/tako). The nice part is that there is no second platform to introduce. If your app runs on Tako, signed image resizing is already in the box.
