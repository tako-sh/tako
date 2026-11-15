# tako-images

Shared image URL signing and transform logic for Tako.

This crate owns the compact path-based signed image URL contract, validation rules, cache policy, and bounded image transform implementation used by `tako-server`. Sources may be JPEG, PNG, GIF, WebP, or AVIF; optimized output is WebP by default or AVIF when requested, with width `1200` as the default maximum resize width. Animated GIF and WebP sources preserve animation for width-only resizes, contain resizes, center-cover crops, and smart-cover crops when emitted as WebP. AVIF output is available for still transforms; animated sources that request AVIF fall back to WebP because Tako does not produce browser-timed AVIF animation. Heightless output width is `min(width, originalWidth)`. Optional height, fit, and crop settings support contain, center-cover, and smart-cover thumbnails without upscaling. EXIF orientation is applied to pixels, but source metadata such as EXIF, XMP, ICC profiles, and comments is stripped from optimized output. Private responses use a 7-day browser-only cache by default and may carry a signed private browser cache `c` max-age override.

## Pipeline

- Decoding: `image` for JPEG, PNG, and still WebP; `image`'s GIF animation adapter for frame composition and disposal; libwebp's reference `WebPAnimDecoder` for animated WebP (disposed areas clear to transparent, as in browsers); statically linked libavif + dav1d for AVIF, including grids, clean aperture, `irot`, and `imir`. Animated AVIF sources decode their first frame.
- Resizing: `fast_image_resize` Lanczos3 in sRGB sample space, premultiplying alpha so transparent pixels do not bleed into edges.
- Cropping: center crops, or an attention-based smart crop that scores edges, saturated color, and skin tones. Animations use one crop window for every frame.
- Encoding: statically linked libwebp for lossy still and animated WebP (photo preset, method 4, sharp YUV, alpha quality equal to quality); `ravif` (rav1e, speed 6, one thread) for AVIF.
- Animation: frame delays are preserved. GIF repeat counts are converted to WebP play counts (`NETSCAPE` repeat `N` becomes `N + 1` plays; no extension plays once; `0` loops forever).
- Limits: source bytes, width, height, and cumulative decoded pixels across frames are checked from headers before pixel data is decoded (for GIF, whose frame count is not in the header, before each frame is kept).

No image library is loaded at runtime. Building the crate needs a C compiler, CMake 3.22 or newer, Meson, Ninja, and NASM on x86.

## Run and Test

From the repository root:

```bash
# Installs CMake, Meson, Ninja, and NASM (apt or Homebrew).
sh scripts/install-image-build-deps.sh
cargo test -p tako-images
```
