---
title: "Animated GIF Optimization with WebP in Tako"
date: "2026-05-21T03:56"
description: "Tako now preserves animation while resizing and cropping GIF and WebP sources through its built-in image optimizer."
image: 72c6017d8f42
imageAlt: "Two octopus mascots feeding animated frames through an image optimization machine."
---

Animated GIFs are tiny movies wearing an image tag costume.

They are also exactly the kind of thing that turns "just optimize images" into a trap. A still JPEG can be resized, cropped, encoded, cached, and forgotten. A GIF has timing, frame count, loop behavior, and a lot of users who will notice immediately if the result becomes a frozen first frame. The optimizer has to treat the animation as part of the image, not as a decoration that can be dropped on the floor.

Tako's image worker now handles animated GIF and WebP sources the same way it handles normal images: validate the source, resize without upscaling, preserve aspect ratio, apply contain or cover crops, encode to WebP, then cache the transformed variant. The difference is that animations stay animated.

## The problem with treating GIFs as pictures

The common failure mode is simple: an optimizer loads only the first frame, applies the usual resize path, and emits a still WebP. The output is smaller, technically valid, and completely wrong.

Animated formats need a slightly different mental model. Libvips represents an animation as a vertical strip of frames with metadata describing each page height, frame delay, and loop behavior. If a GIF has 24 frames at `540x405`, the decoded image can look like one `540x9720` image internally. The optimizer has to use the per-frame height for resize math, dimension limits, crop placement, and output dimensions, while still accounting for total decoded pixels so a huge animation cannot sneak through as "one image."

That is why Tako now loads animated GIF and animated WebP sources with all pages, not just the first page. The transform worker calculates frame dimensions from libvips metadata, applies resize and crop operations per frame when needed, then saves WebP output with the correct page height so the output remains an animation.

The result is boring in the good way:

| Source        | Width-only resize | Contain resize | Center cover crop | Smart cover crop | Result                |
| ------------- | ----------------- | -------------- | ----------------- | ---------------- | --------------------- |
| Still images  | Yes               | Yes            | Yes               | Yes              | Still optimized image |
| Animated GIF  | Yes               | Yes            | Yes               | Yes              | Animated WebP         |
| Animated WebP | Yes               | Yes            | Yes               | Yes              | Animated WebP         |

Public image URLs still start with the small `imageUrl()` helper from `tako.sh`, which builds `/_tako/image?src=...&w=...` URLs for cacheable page images. The full transform engine underneath that endpoint also understands height, fit, and crop settings, and deployed servers run that work through the same isolated image worker pool described in the [deployment docs](/docs/deployment).

```ts
import { imageUrl } from "tako.sh";

const loop = imageUrl("/assets/spinner.gif", {
  width: 640,
});
```

The app still declares intent: source, width, optional quality, optional format. `tako-server` enforces the configured guardrails from [`tako.toml`](/docs/tako-toml), fetches or reads the original, does the libvips work, and serves a cacheable result from your own route.

## What the optimizer preserves

For animations, the important bit is not only "we got a smaller file." The important bit is that the file is still the same kind of user experience.

The comparison below uses the GIF sample from this test. The original is an 88-frame GIF at `540x405`. The optimized version keeps the same `540x405` dimensions and emits animated WebP at quality 75. The visual detail changes because the codec changed; the motion, timing, frame count, and canvas size survive.

<div style="display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:16px;margin:20px 0;">
  <figure style="margin:0;">
    <img src="/assets/blog/gif-optimization/original-animation.gif" alt="Original animated GIF sample used to test Tako image optimization." loading="lazy" width="540" height="405" />
    <figcaption><strong>Original GIF</strong><br />540x405, 88 frames, 5.2 MB</figcaption>
  </figure>
  <figure style="margin:0;">
    <img src="/assets/blog/gif-optimization/optimized.webp" alt="Optimized animated WebP version of the same GIF sample." loading="lazy" width="540" height="405" />
    <figcaption><strong>Optimized WebP</strong><br />540x405, 88 frames, q75, 2.0 MB</figcaption>
  </figure>
</div>

## Why no animated AVIF yet

I tested animated AVIF with local libvips 8.18.2 using the same `540x405` source and the same `q75` quality setting. It was slower, about 8.4 seconds versus about 2 seconds for WebP. It was larger for this sample, 2.8 MB versus roughly 2 MB. It was also not browser-correct: libvips could reload it as 88 pages, but Chromium rendered it as a still image. The current [libvips multipage and animated image docs](https://www.libvips.org/API/current/multipage-and-animated-images.html) draw the same practical line: GIF, WebP, and JXL are animation-capable savers, while AVIF is multipage-capable. So Tako keeps the product rule simple: still images can use AVIF, but animated sources fall back to animated WebP when needed.

The same source can also be center-cropped or smart-cropped frame by frame. That matters for animated avatars, product loops, reaction stickers, loading loops, tutorial snippets, and anything else where the animation is short enough to live in an image element but still needs the same thumbnail shapes as still images.

| Variant                   | Dimensions | Quality | Frames | Size   | What changed                         |
| ------------------------- | ---------- | ------- | ------ | ------ | ------------------------------------ |
| Original GIF              | 540x405    | Source  | 88     | 5.2 MB | Baseline source                      |
| Same-size animated WebP   | 540x405    | 75      | 88     | 2.0 MB | WebP encode, animation preserved     |
| Smart-cover animated WebP | 320x320    | 75      | 88     | 1.1 MB | Per-frame crop to a square thumbnail |

Tiny flat-color loops, noisy screen recordings, and photographic clips all behave differently. The point is that Tako gives animated GIFs and WebPs the same controlled path: allowed dimensions, allowed formats, quality settings, byte limits, decoded pixel limits, no upscaling, metadata stripping, and transform cache keys that include the source bytes and options.

## Where this fits in Tako

Image optimization sits in the same product layer as [local development](/docs/development), routing, TLS, storage URLs, channels, and workflows. Your application decides which image belongs on the page. Tako owns the platform boundary around that decision: validation, source loading, transform work, response headers, and cache behavior.

```d2
direction: right

app: "App code\nchooses /assets/spinner.gif" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
url: "Optimizer URL\n/_tako/image?src=...&w=..." {
  style.fill: "#9BC4B6"
}
worker: "Image worker\nlibvips" {
  style.fill: "#E88783"
}
strip: "Animation frames\npage-height + delay" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}
cache: "Transform cache\nWebP animation" {
  style.fill: "#9BC4B6"
}
browser: "Browser\nanimated img" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

app -> url: "render img src"
url -> worker: "validated request"
worker -> strip: "load all frames"
strip -> worker: "resize or crop per frame"
worker -> cache: "save animated output"
cache -> browser: "cacheable response"
```

The deployed server keeps source bytes briefly in memory so one page can request multiple variants without fetching the same original repeatedly. Successful transforms are cached on disk under the system temp directory. Cache hits and duplicate in-flight misses do not enter the worker queue, and new misses use the managed image worker pool so resize and encode work does not consume the main proxy process budget.

That is more than "convert GIF to WebP." It is the platform doing the careful parts that are easy to forget when image optimization starts as a helper function in one route.

## The practical shape

Use GIF sources when that is what your app already has. Ask Tako for the size you actually render. Prefer WebP for animated output. Still-image format choices keep working as configured, and animated sources keep the motion by returning animated WebP when needed. Keep remote sources behind [`[images].remote_patterns`](/docs/tako-toml#images) so the optimizer can fail closed.

For still images, none of this changes the normal path. For animated images, the path finally stops being special. A short GIF can become an animated WebP thumbnail or a square smart-cropped avatar without losing the part users care about: it still moves.

That is the kind of infrastructure we want Tako to absorb. Not a giant media pipeline. Just the common image work your app needs, running inside the same self-hosted boundary as deploys, HTTPS, routing, and cache policy. The full behavior is documented in [How Tako Works](/docs/how-tako-works), and the implementation is in the [Tako repo](https://github.com/lilienblum/tako) if you want to follow the frame strip all the way down.
