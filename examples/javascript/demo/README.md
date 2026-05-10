# Moonbase Supply Desk — Tako demo

A TanStack Start demo app that doubles as a live tour of Tako's primitives: **multi-tenancy**, **durable workflows**, and **channels**.

Each moonbase is an isolated tenant (wildcard subdomain). Submitting a supply request enqueues a five-step sequential workflow (check → pack → load → ship → deliver) where the late steps occasionally throw and Tako retries them via `step.run`'s `retries` option. Every step publishes to the `mission-log` channel, so the right-rail log streams live to every connected client. A daily cron workflow deletes demo database records older than three days.

Live at [demo.tako.sh](https://demo.tako.sh).

## Local Dev (Vite)

```bash
cd examples/javascript/demo
bun install
bun run dev
```

This mode works without a Tako runtime. It uses an in-process simulator that publishes mission-log events on the same cadence as the real workflow, so you can try the UI in plain Vite.

## Run With Tako Dev Flow

From the demo app directory:

```bash
cd examples/javascript/demo
tako dev
```

Use this mode for the real Tako path: workflows are enqueued through the internal socket and events flow through the actual `mission-log` channel instead of the local simulator.

Import demo secrets (dev, then prod): `printf '%s\n' 'eyJ2ZXJzaW9uIjoxLCJpZCI6IjlhYzhkYjk1N2MwZTQwNjEiLCJrZXkiOiI2WjhwazZnWElsN1A2ZGlNaTVDRFN0cEdXWmhUQVI5Tnp0RWR6RXZPUWxZPSJ9' | tako secrets key import --env development && printf '%s\n' 'eyJ2ZXJzaW9uIjoxLCJpZCI6IjJmMzJmMzFhZmQyMDQ1MmEiLCJrZXkiOiJWTVl0U2JXVmRuSXhISWV0dCtyS2ZiK3JPU0laYzNrUE1mS3ZSbUlqY2tJPSJ9' | tako secrets key import --env production`

## Build

```bash
cd examples/javascript/demo
bun run build
```

## Test

```bash
cd examples/javascript/demo
bun test
```

## Notes

- `tako.toml` sets `preset = "tanstack-start"` with `runtime = "bun"`.
- Plain `bun run dev` uses an in-process simulator for the workflow.
- `tako dev` uses real Tako channels + workflows.
- The cleanup cron workflow runs daily and removes supply requests older than three days plus empty stale bases.
- Tenant is detected server-side from the `Host` header — no env var needed.
  - `artemis-prime.demo.tako.sh` → tenant `artemis-prime` (Mission Control view)
  - `demo.tako.sh` → no tenant (Landing view with base-name input)
- Development routes: `demo.test`, `*.demo.test`
- Production routes: `demo.tako.sh`, `*.demo.tako.sh`

## Files of interest

- `workflows/order-shipment.ts` — five-step sequential workflow with `step.run` retries
- `workflows/cleanup.ts` — daily scheduled cleanup for old demo DB rows
- `channels/mission-log.ts` — pub/sub channel for live events
- `src/routes/index.tsx` — route glue, server loader, local-mode simulator
- `src/server/db.ts` — SQLite persistence and retention cleanup
- `src/components/moonbase/` — all UI components (MissionControl, Landing, Sidebar, etc.)
- `src/styles/app.css` — Tailwind v4 `@theme` with the Obsidian Observatory palette
