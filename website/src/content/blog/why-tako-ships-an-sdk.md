---
title: "Why Tako Ships an SDK"
date: "2026-04-04T12:00"
description: "Most deploy tools stop at infrastructure. Tako's SDK gives your app readiness signaling, secret injection, and runtime abstraction — here's why."
image: 7f4bb724896c
---

Most deploy tools are infrastructure-only. They move your code to a server, maybe configure a reverse proxy, and call it a day. What happens inside your app process is your problem.

We think that's leaving a lot on the table.

Tako ships SDKs for [JavaScript/TypeScript](https://www.npmjs.com/package/tako.sh), [Go](https://pkg.go.dev/tako.sh), and [Rust](https://crates.io/crates/tako) because deployment depends on cooperation between infrastructure and application code. Readiness signaling, secret injection, health checks, graceful shutdown, and calls between apps all cross that boundary.

## What the SDK Actually Does

The SDK is thin. Your app exports a [fetch handler](https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API), and the SDK turns it into a running server:

```typescript
// index.ts — that's it, that's your app
export default function fetch(request: Request): Response {
  return new Response("Hello from Tako");
}
```

Under the hood, the SDK handles everything between your code and the infrastructure:

| Concern             | Without SDK                                   | With Tako SDK                                                 |
| ------------------- | --------------------------------------------- | ------------------------------------------------------------- |
| Server binding      | You configure host/port, pick a framework     | SDK binds to an OS-assigned port automatically                |
| Readiness signal    | Infra polls a TCP port and hopes for the best | App signals `TAKO:READY` when _actually_ ready                |
| Health checks       | You build a `/health` endpoint                | Built-in `/status` endpoint with uptime, version, instance ID |
| Secrets             | `.env` files or external secret managers      | Injected via fd 3 before your code runs, auto-redacted on log |
| Graceful shutdown   | You handle `SIGTERM` yourself                 | SDK drains in-flight requests, then exits cleanly             |
| Runtime portability | Locked to one runtime's HTTP API              | Same `fetch()` signature across Bun and Node                  |

## The Readiness Problem

This is the big one. Most deploy tools check health by poking a TCP port — if the socket accepts connections, the app must be ready. But that's not true. Your server might be listening while still loading config, warming caches, or running migrations.

Tako's approach is different. The SDK signals readiness explicitly:

```d2
direction: down

server: tako-server
lb: Load Balancer

app: App + SDK {
  startup: SDK startup {
    secrets: Read secrets from fd 3
    code: Import user code
    bind: Bind to port

    secrets -> code -> bind
  }
}

server -> app.startup.secrets: spawn (PORT=0)
app.startup.bind -> server: "TAKO:READY:12345"
server -> app: probe /status
server -> lb: add instance
lb -> app: route traffic
```

The new instance writes `TAKO:READY:<port>` to stdout only after it has read secrets, imported your code, and bound to a port. The server waits for that signal, runs a health probe, and only then adds the instance to the [load balancer](/docs/deployment/). No dropped requests, no 502s.

## Secrets That Don't Leak

Secrets deserve special mention. Tako encrypts secrets with AES-256-GCM and [stores them in your repo](/docs/cli/). At deploy time, the server passes them to your app through file descriptor 3 — not environment variables, not files on disk.

The SDK reads fd 3 _before importing your code_, then exposes secrets through a `Proxy` that redacts itself. `tako generate` emits a project-local `tako.d.ts` that types `tako.secrets` from `tako.sh`:

```typescript
import { tako } from "tako.sh";

tako.secrets.DATABASE_URL; // → "postgres://..."
console.log(tako.secrets); // → "[REDACTED]"
JSON.stringify(tako.secrets); // → "\"[REDACTED]\""
```

Accidental logging? Handled. Serialization into error reports? Handled.

## Same Code, Dev and Prod

One of the quieter benefits: [`tako dev`](/docs/development/) runs your app through the exact same SDK entrypoint as production. Same fetch handler wrapping, same readiness protocol, same health endpoint. The only differences are the ones you'd expect — local HTTPS on `.test` instead of your production domain, debug log level by default.

This means if your app starts correctly in dev, it starts correctly in production. No "works on my machine" surprises from a different server setup.

<a id="the-real-reason-what-comes-next"></a>

## The Real Reason: Built-In Primitives

The same integration gives applications durable work, realtime delivery, and private calls to other apps.

Without an SDK, a deploy tool can only manage processes from the outside — start, stop, route traffic. It has no way to coordinate with your application code. That's fine for basic deploys, but it's a dead end for anything more interesting.

The JavaScript SDK exposes these capabilities alongside the HTTP runtime:

- **Background jobs** — define a job handler in your code, trigger it from any request, let Tako manage the queue and retries server-side. No external job queue to run.
- **Durable workflows** — long-running, resumable workflows where Tako persists state across steps. Your code defines the steps, the SDK handles checkpointing and recovery.
- **Scheduled tasks** — cron-like tasks declared in code, executed by the server. Type-safe, version-deployed, no separate cron service.
- **Durable channels** — WebSocket/SSE routes for connected browsers, plus bounded replay for reconnects. JavaScript apps keep the file-based shape: `channels/` for durable realtime. The SDK handles connection lifecycle, auth, and message typing.
- **Network handlers** — one default-exported function per file in `network-handlers/`, called with `call("users.lookup", input)`. It executes without enqueueing and returns JSON. Workflows and channels also use `app.primitive` addresses through `workflow(...)` and `channel(...)`.

Each of these features needs both sides of the equation: server-side infrastructure _and_ application-level integration. A deploy tool without an SDK would need you to bring your own job queue, your own workflow engine, your own scheduler. The SDK is what lets Tako offer these as built-in primitives — type-safe, zero-config, and deployed alongside your app.

Calls stay in the caller's network and environment. Direct handlers have no automatic retries. Workflow `.run(payload)` enqueues and waits for a retained result; inside a workflow it checkpoints the child call so the parent can resume after a restart. Channel publication acknowledges publication without waiting for subscribers.

Run `tako gen` to export the app's `tako-schema.json`. A consumer lists producer files or HTTP(S) URLs in root-level `schemas` in `tako.toml` and generates its own types. TypeScript gets `tako.imports.ts` and address registries in `tako.d.ts`; Go and Rust get native DTOs and constants. Commit `tako-schema.lock` for reproducible URL imports. Generation does not change package exports.

Go and Rust callers use the same addresses and can import/export schemas, while recipient dispatch and automatic durable child calls currently require JavaScript. See [Network Primitives](/docs/network-primitives/) for the language matrix and complete examples.

## A Thin Layer, Not a Framework

The SDK integrates with your HTTP framework and database choices. You can use [Hono](https://hono.dev), [Elysia](https://elysiajs.com), Express, or raw `Request`/`Response`. Its primitive folders define discoverable application entry points; they do not replace your application's router or data layer.

The public API includes the runtime context, workflow and channel definitions, network calls, and typed configuration. A plain fetch handler remains ordinary HTTP code. Applications using durable primitives also depend on their persistence and delivery behavior, so moving those features requires replacing that behavior as well as the imports.

We think the best infrastructure is the kind that knows just enough about your app to do its job well — and nothing more. That's why Tako ships an SDK.

Check out the [docs](/docs/) to get started, or read more about [how Tako works](/docs/how-tako-works/) under the hood.
