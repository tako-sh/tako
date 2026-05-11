# tako-images

Shared image URL signing and transform logic for Tako.

This crate owns the compact path-based signed image URL contract, validation rules, cache policy, and bounded libvips-backed image transform implementation used by `tako-server`. Sources may be JPEG, PNG, WebP, or AVIF; optimized output is AVIF by default or WebP when requested, with width `1200` as the default maximum resize width. Heightless output width is `min(width, originalWidth)`. Optional height, fit, and crop settings support contain, center-cover, and smart-cover thumbnails without upscaling. Private responses use a 7-day browser-only cache by default and may carry a signed private browser cache `c` max-age override.

## Run and Test

From the repository root:

```bash
# Requires libvips. On macOS: brew install vips
cargo test -p tako-images
```
