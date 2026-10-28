---
title: Choose a Framework
description: "Choose a verified Tako integration for Next.js, TanStack Start, Vite, fetch handlers, Go HTTP frameworks, or Rust/Axum, with SDK support boundaries."
---

Choose the page matching the existing application. Keep its router and business code; connect Tako at the server entrypoint or listener boundary. Each guide covers configuration, runtime integration, and verification.

## Choose a guide

| Application                   | Guide                              | Integration                                                          |
| ----------------------------- | ---------------------------------- | -------------------------------------------------------------------- |
| Next.js                       | [Next.js](/nextjs/)                | `withTako()` and the `nextjs` preset                                 |
| React TanStack Start          | [TanStack Start](/tanstack-start/) | Tako Vite plugin and the `tanstack-start` preset                     |
| Other Vite application        | [Vite](/vite/)                     | Dev readiness and explicitly verified fetch-compatible server output |
| Plain Bun or Node.js HTTP app | [Fetch Handlers](/fetch-handler/)  | Web Request/Response handler and SDK entrypoint                      |
| Go standard library           | [Go net/http](/go-http/)           | `tako.ListenAndServe(http.Handler)`                                  |
| Gin                           | [Gin](/gin/)                       | Pass the Gin engine to the Go SDK                                    |
| Echo                          | [Echo](/echo/)                     | Pass the Echo application to the Go SDK                              |
| Chi                           | [Chi](/chi/)                       | Pass the Chi router to the Go SDK                                    |
| Rust/Axum                     | [Rust and Axum](/axum/)            | `tako::axum::serve` with the `axum` feature                          |

For an image-based release, use the [Docker section](/docker/). A Dockerfile changes packaging; the app still needs its SDK's bootstrap and authenticated status handling.

## Pick a preset first

Built-in JavaScript framework presets are `nextjs`, `tanstack-start`, and `vite`. The Vite preset supplies a dev command, not a universal production server adapter. Go has a runtime preset rather than framework-specific entries. Rust uses explicit native build/start commands or a container release; `runtime = "rust"` is not a supported configuration value.

See [Presets](/presets/) for defaults. A framework being built with Vite does not establish that its server output implements the fetch contract. For frameworks not listed here, inspect and adapt their actual server exports or listener integration before choosing `main`.

## Language and SDK support

| Capability                                                | JavaScript / TypeScript                                                         | Go                                               | Rust                            |
| --------------------------------------------------------- | ------------------------------------------------------------------------------- | ------------------------------------------------ | ------------------------------- |
| HTTP runtime and app secrets                              | `tako.sh`                                                                       | Root `tako.sh` module                            | `tako` crate                    |
| Presence and Store backends                               | Definitions discovered under `app_root`                                         | Explicit registrations                           | Explicit registries             |
| Workflow handlers and workers                             | `defineWorkflow`                                                                | Separate `github.com/tako-sh/tako/sdk/go` module | No workflow SDK API currently   |
| Storage URL helpers, image URL helpers, server JSON cache | JavaScript SDK helpers                                                          | No equivalent helpers currently                  | No equivalent helpers currently |
| Browser realtime and React hooks                          | Shared `tako.sh/client` and `tako.sh/react` clients, including Go/Rust backends | Same browser clients                             | Same browser clients            |

Code groups on multi-language pages show supported equivalents. Command-line configuration is shared, while SDK function names and feature availability differ. The individual [SDK skills](https://tako.sh/skills.md) provide language-specific details.

## TanStack Start (and other Vite SSR frameworks)

Use [TanStack Start](/tanstack-start/) for the supported preset and full configuration. For another Vite framework, follow [Vite](/vite/) and verify that the emitted server entry accepts a Request and returns a Response.

## Next.js

The dedicated [Next.js guide](/nextjs/) covers `withTako()`, the server entry, static assets, local HTTPS, and the shared image optimizer.

## Fallback: fetch handler (no preset)

Use [Fetch Handlers](/fetch-handler/) for a small Bun or Node.js service. Native Go and Rust apps instead integrate through their SDK listener. Do not add the Vite plugin to a plain fetch app just to make Tako run it.
