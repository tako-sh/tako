# E2E Next.js Fixture

Minimal Next.js App Router fixture used by Docker deploy e2e tests.

## Run Deploy E2E Test

From repo root:

```bash
just e2e e2e/fixtures/javascript/nextjs
```

That runs the global Docker harness (`e2e/run.sh`) against this fixture path.

## Notes

- Uses the `nextjs` preset with `runtime = "node"` and `package_manager = "bun"`.
- `next.config.mjs` uses `withTako` from `tako.sh/nextjs`.
- The app page uses `next/image`; `withTako` configures the global Tako image loader.
- The build emits `.next/tako-entry.mjs`. If Next emits standalone output, the wrapper uses it; otherwise it falls back to `next start`.
- Post-deploy checks verify health JSON, root HTML response, and Next static asset reachability.
