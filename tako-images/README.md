# tako-images

Shared image URL signing and transform logic for Tako.

This crate owns the path-based signed image URL contract, validation rules, cache policy, and bounded JPEG/PNG resize implementation used by `tako-server`.

## Run and Test

From the repository root:

```bash
cargo test -p tako-images
```
