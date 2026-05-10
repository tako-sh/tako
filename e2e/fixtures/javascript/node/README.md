# E2E Node Fixture

Minimal Node API-style fixture app used by Docker deploy e2e tests.

## Run Deploy E2E Test

From repo root:

```bash
just e2e e2e/fixtures/javascript/node
```

## Notes

- This fixture is source-only (no local build step).
- `tako.toml main` points at `index.ts`, and `tako-server` launches it through the Tako SDK wrapper.
- The app root returns minimal HTML (`<h1>Tako app</h1>`), and internal health is handled via `Host: node-e2e.tako` + `/status`.
