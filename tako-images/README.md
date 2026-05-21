# tako-images

Shared image URL signing and transform logic for Tako.

This crate owns the compact path-based signed image URL contract, validation rules, cache policy, and bounded libvips-backed image transform implementation used by `tako-server`. Sources may be JPEG, PNG, GIF, WebP, or AVIF; optimized output is WebP by default or AVIF when requested, with width `1200` as the default maximum resize width. Animated GIF and WebP sources preserve animation for width-only resizes, contain resizes, center-cover crops, and smart-cover crops when emitted as WebP. AVIF output is available for still transforms; animated sources that request AVIF fall back to WebP because the current libvips HEIF save path supports multipage AVIF output, not browser-timed AVIF animation. Heightless output width is `min(width, originalWidth)`. Optional height, fit, and crop settings support contain, center-cover, and smart-cover thumbnails without upscaling. EXIF orientation is applied to pixels, but source metadata such as EXIF, XMP, ICC profiles, and comments is stripped from optimized output. Private responses use a 7-day browser-only cache by default and may carry a signed private browser cache `c` max-age override.

## Run and Test

From the repository root:

```bash
# Requires libvips. On macOS: brew install vips
cargo test -p tako-images
```
