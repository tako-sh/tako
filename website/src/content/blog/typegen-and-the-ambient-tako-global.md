---
title: "tako typegen and the generated tako.gen.ts"
date: "2026-04-18T01:01"
description: "Tako generates a project-local tako.gen.ts with typed runtime state and a typed secrets bag — zero ambient globals, zero module augmentation, zero silent typos."
image: 402b4ee3a4e4
---

Most runtime config is reached through APIs that lie to you. `process.env` pretends every variable is a string and returns `undefined` when you typo a name. `process.env.DATBASE_URL` is a syntactically valid read that fails silently, then explodes somewhere downstream — usually at 2am, usually in production.

Tako's JavaScript SDK ships a different shape. `tako typegen` writes a `tako.gen.ts` file into your project with typed exports for every secret, runtime value, and log handle you'll reach for. No ambient globals, no module augmentation, no guessing — just ES modules.

## What the generated file gives you

Every Tako JS/TS project has a `tako.gen.ts` managed by the CLI. It's a real `.ts` file, not a `.d.ts` ambient declaration — you can open it, read it, and see exactly what's exported.

```ts
// Anywhere in your app
import { env, isDev, port, dataDir, build, logger, secrets } from "../tako.gen";

secrets.DATABASE_URL; // typed string
env; // "development" | "production" | undefined
isDev; // boolean
port; // number, assigned by Tako
dataDir; // persistent path, survives deploys
build; // deploy-time build ID
logger.info("hello", { userId });
```

Channels and workflows aren't on the runtime context — they're regular modules you import from their own files:

```ts
import sendEmail from "../workflows/send-email";
import chat from "../channels/chat";

await sendEmail.enqueue({ to });
await chat({ roomId }).publish({ type: "msg", data: { text, userId } });
```

Same shape on Bun and Node. No global install step, no runtime Proxy, no kebab↔camel rule to remember.

## What `tako typegen` generates

[`tako typegen`](/docs/cli) scans your project and writes a single file:

| Source                           | What typegen emits                                                             |
| -------------------------------- | ------------------------------------------------------------------------------ |
| `.tako/secrets.json` (encrypted) | `interface Secrets { readonly DATABASE_URL: string; ... }` + `secrets` export  |
| Runtime env                      | `env`, `isDev`, `isProd`, `port`, `host`, `build`, `dataDir`, `appDir` exports |
| App infra                        | `logger` export (structured JSON logger bound to `source: "app"`)              |

Secret names are plaintext in [`.tako/secrets.json`](/blog/secrets-without-env-files) — the values aren't — so typegen emits the type surface without ever touching your encryption key. When you add a secret with `tako secrets set`, typegen picks it up and rewrites `tako.gen.ts` on the next `tako dev`, `tako deploy`, or `tako typegen`.

The file lands somewhere TypeScript's default `include` will find: next to an existing copy if you have one, or inside `src/` or `app/` if those directories exist, or at the project root. No `tsconfig.json` edits needed.

## Why this is safer than `process.env`

`process.env` is fundamentally a `string → string` map. `process.env.DATBASE_URL` is a valid read; it just returns `undefined`. Your editor can't warn you because the shape of `process.env` isn't tied to your actual secrets.

`secrets.DATBASE_URL` is a compile error. `import foo from "../workflows/bar"` where `bar.ts` doesn't exist is a compile error. If TypeScript sees your file, it'll catch these before they ever run.

A few more guarantees:

- **Redaction by default.** `String(secrets)` returns `"[REDACTED]"`. `JSON.stringify(secrets)` returns `"[REDACTED]"`. Log the whole object by accident and no values leak.
- **Server-only.** The generated file is evaluated on the Node/Bun process that runs your entrypoint; the browser has its own universe. Browser code pulls from `tako.sh/client` or `tako.sh/react` instead.
- **Plain ES modules.** No `declare global`, no Proxy, no module augmentation. Rename-safe, tree-shakeable, jumps-to-definition, mocks cleanly in tests.

## Try it

`tako typegen` runs automatically during [`tako init`](/docs/cli), [`tako dev`](/docs/development), [`tako deploy`](/docs/deployment), and `tako secrets ...`. Most of the time you don't think about it. When you want types updated manually:

```bash
tako secrets set STRIPE_KEY --env production
tako typegen
# Generated tako.gen.ts
```

For Go apps, typegen emits a `tako_secrets.go` with a typed `Secrets` struct — same idea, same compile-time catch. See [the Go SDK post](/blog/the-go-sdk-is-here) for the shape of that side.

Typed runtime config isn't a new idea. Getting it for secrets and runtime state with zero ceremony — no ambient globals, no module augmentation, just a `.ts` file you import — is what `tako typegen` is for.
