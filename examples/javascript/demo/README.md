# Planetary Supply Desk — Tako demo

A TanStack Start demo app that doubles as a live tour of Tako's primitives: **wildcard routing**, **durable workflows**, **channels**, and **image optimization**.

Each planet base has a mission page at `<base>.demo.tako.sh`, with `/bases/<base>` kept as a local fallback. Submitting a supply request enqueues a five-step sequential workflow (check → pack → load → ship → deliver) where the late steps occasionally throw and Tako retries them via `ctx.run`'s `retries` option. Every step publishes to the `mission-log` channel, so the right-rail log streams live to every connected client. Base artwork lives in `public/images/`; the app uses `imageUrl()` so Tako serves resized, cached images from `/_tako/image`. A daily cron workflow deletes demo database records older than three days.

Live at [demo.tako.sh](https://demo.tako.sh).

## Local Dev (Vite)

```bash
cd examples/javascript/demo
bun install
bun run dev
```

This mode works without a Tako runtime for route and UI checks. Workflow enqueueing, live channels, and image optimization require `tako dev`.

## Run With Tako Dev Flow

From the demo app directory:

```bash
cd examples/javascript/demo
tako dev
```

Use this mode for the real Tako path: workflows are enqueued through the internal socket, events flow through the actual `mission-log` channel, and base artwork is served through `/_tako/image`.

Import demo development secrets:

```bash
printf '%s\n' 'eyJ2ZXJzaW9uIjoxLCJpZCI6IjlhYzhkYjk1N2MwZTQwNjEiLCJrZXkiOiI2WjhwazZnWElsN1A2ZGlNaTVDRFN0cEdXWmhUQVI5Tnp0RWR6RXZPUWxZPSJ9' | tako secrets key import --env development
```

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
- Plain `bun run dev` is UI-only for mission routes; use `tako dev` to exercise channels and workflows.
- `tako dev` uses real Tako channels + workflows.
- The cleanup cron workflow runs daily and removes supply requests older than three days plus empty stale bases.
- Base context is read server-side from the wildcard host or the `/bases/<base>` fallback route, so no env var is needed.
  - `valles-hub.demo.tako.sh` → base `valles-hub` (Mission Control view)
  - `demo.tako.sh` → landing view with base-name input
- Development routes: `demo.test`, `*.demo.test`
- Production routes: `demo.tako.sh`, `*.demo.tako.sh` (DNS-only/direct to the Tako server)

## Files of interest

- `src/workflows/order-shipment.ts` — five-step sequential workflow with `ctx.run` retries
- `src/workflows/cleanup.ts` — daily scheduled cleanup for old demo DB rows
- `src/channels/mission-log.ts` — pub/sub channel for live events
- `src/routes/index.tsx` — landing route, server loader, image URL signing
- `src/routes/` — landing, base mission-control, and OG routes
- `src/components/mission-controller.tsx` — workflow enqueueing and live channel state
- `src/server/db.ts` — SQLite persistence and retention cleanup
- `src/lib/bases.ts` — planet base catalog and source image paths
- `src/lib/images.ts` — Tako image URLs for base artwork
- `public/images/` — generated source artwork for the base previews
- `src/components/` — all UI components (MissionControl, Landing, Sidebar, etc.)
- `src/styles/app.css` — Tailwind v4 `@theme` with the Obsidian Observatory palette
