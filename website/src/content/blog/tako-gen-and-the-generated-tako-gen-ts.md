---
title: "tako generate and the generated tako.d.ts"
date: "2026-04-18T01:01"
description: "Tako generates a project-local tako.d.ts that types the tako runtime object from tako.sh — no app global, no silent typos."
image: 75030c2f757f
---

Most runtime config is reached through APIs that lie to you. `process.env` pretends every variable is a string and returns `undefined` when you typo a name. `process.env.DATBASE_URL` is a syntactically valid read that fails silently, then explodes somewhere downstream — usually at 2am, usually in production.

Tako's JavaScript SDK ships a different shape. App code imports `tako` from `tako.sh`, and `tako generate` writes a project-local `tako.d.ts` file that teaches TypeScript your secret keys, environment names, channel metadata, and workflow metadata. No app global, no guessing — just ES modules.

## What the generated file gives you

Every Tako JS/TS project has a `tako.d.ts` managed by the CLI. App code does not import it directly; TypeScript includes it and uses it to augment the public `tako.sh` package.

```ts
import { tako } from "tako.sh";

tako.secrets.DATABASE_URL; // typed string
tako.env; // "development" | "production" | undefined
tako.isDev; // boolean
tako.port; // number, assigned by Tako
tako.dataDir; // persistent path, survives deploys
tako.build; // deploy-time build ID
tako.logger.info("hello", { userId });
```

Channels and workflows aren't on the runtime context — they're regular modules you import from their own files:

```ts
import sendEmail from "../workflows/send-email";
import chat from "../channels/chat";

await sendEmail.enqueue({ to });
await chat({ roomId }).publish({ type: "msg", data: { text, userId } });
```

Same shape on Bun and Node. No global install step, no kebab↔camel rule to remember.

## What `tako generate` generates

[`tako generate`](/docs/cli) scans your project and writes a single file:

| Source                           | What `tako generate` emits                                                      |
| -------------------------------- | ------------------------------------------------------------------------------- |
| `.tako/secrets.json` (encrypted) | `interface TakoSecrets { readonly DATABASE_URL: string; ... }`                  |
| `tako.toml` envs                 | <code>type Env = "development" \| "production" \| "staging"</code>              |
| Channel files                    | `interface TakoChannels { ... }` metadata for discovered channel definitions    |
| Workflow files                   | `interface TakoWorkflows { ... }` metadata for discovered workflow definitions  |
| Runtime env                      | `process.env` / `import.meta.env` declarations for Tako-provided runtime values |

Secret names are plaintext in [`.tako/secrets.json`](/blog/secrets-without-env-files) — the values aren't — so `tako generate` emits the type surface without ever touching your encryption key. When you add a secret with `tako secrets set`, `tako.d.ts` picks it up on the next `tako dev`, `tako deploy`, or `tako generate`.

The file lands somewhere TypeScript's default `include` will find: next to an existing copy if you have one, or inside `src/` or `app/` if those directories exist, or at the project root. No `tsconfig.json` edits needed.

## Why this is safer than `process.env`

`process.env` is fundamentally a `string → string` map. `process.env.DATBASE_URL` is a valid read; it just returns `undefined`. Your editor can't warn you because the shape of `process.env` isn't tied to your actual secrets.

`tako.secrets.DATBASE_URL` is a compile error. `import foo from "../workflows/bar"` where `bar.ts` doesn't exist is a compile error. If TypeScript sees your file, it'll catch these before they ever run.

A few more guarantees:

- **Redaction by default.** `String(tako.secrets)` returns `"[REDACTED]"`. `JSON.stringify(tako.secrets)` returns `"[REDACTED]"`. Log the whole object by accident and no values leak.
- **Type-only generation.** The generated file has declarations, not runtime code. Runtime state comes from `tako.sh`.
- **Plain ES modules.** Import `tako` from `tako.sh`. Rename-safe, jumps-to-definition, mocks cleanly in tests.

## Try it

`tako generate` runs automatically during [`tako init`](/docs/cli), [`tako dev`](/docs/development), [`tako deploy`](/docs/deployment), and `tako secrets ...`. Most of the time you don't think about it. `tako gen` and `tako g` are aliases. When you want types updated manually:

```bash
tako secrets set STRIPE_KEY --env production
tako generate
# Generated tako.d.ts
```

For Go apps, `tako generate` emits a `tako_secrets.go` with a typed `Secrets` struct — same idea, same compile-time catch. See [the Go SDK post](/blog/the-go-sdk-is-here) for the shape of that side.

Typed runtime config isn't a new idea. Getting it for secrets and runtime state with zero ceremony — no app global, just `tako.sh` plus a generated declaration file — is what `tako generate` is for.
