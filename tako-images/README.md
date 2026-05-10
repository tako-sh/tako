# tako-images

Shared image URL signing and transform logic for Tako.

This crate owns the path-based signed image URL contract, validation rules, cache policy, and bounded libvips-backed JPEG/PNG/WebP resize implementation used by `tako-server`.

## Run and Test

From the repository root:

```bash
# Requires libvips. On macOS: brew install vips
cargo test -p tako-images
```
