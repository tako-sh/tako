---
title: "tako generate and the generated tako.d.ts"
date: "2026-04-18T01:01"
description: "Generate local Tako declarations and language-neutral schemas for typed network handlers, workflows, and channels across TypeScript, Go, and Rust."
image: 75030c2f757f
---

Most runtime config is reached through APIs that lie to you. `process.env` pretends every variable is a string and returns `undefined` when you typo a name. `process.env.DATBASE_URL` is a syntactically valid read that fails silently, then explodes somewhere downstream — usually at 2am, usually in production.

Tako's JavaScript SDK ships a different shape. App code imports `tako` from `tako.sh`, and `tako generate` writes a project-local `tako.d.ts` file that teaches TypeScript your secret keys, environment names, channel metadata, and workflow metadata. No app global, no guessing — just ES modules.

## What the generated file gives you

Every Tako JS/TS project has a `tako.d.ts` managed by the CLI. App code does not import it directly; TypeScript includes it and uses it to augment the public `tako.sh` package.

```ts
import { tako } from "tako.sh";

tako.secrets.DATABASE_URL; // typed string
tako.env; // configured environment union, including "development" and "production"
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

Channel params are connection context, not room isolation: every authorized binding to the same channel name reads the same stream. See the [Channels reference](/docs/channels/#understand-channel-params) before publishing private events.

Same shape on Bun and Node. No global install step, no kebab↔camel rule to remember.

## What `tako generate` generates

[`tako generate`](/docs/cli/) scans your project and updates its local declarations:

| Source                           | What `tako generate` emits                                                     |
| -------------------------------- | ------------------------------------------------------------------------------ |
| `.tako/secrets.json` (encrypted) | `interface TakoSecrets { readonly DATABASE_URL: string; ... }`                 |
| `tako.toml` envs                 | <code>type Env = "development" \| "production" \| "staging"</code>             |
| Channel files                    | `interface TakoChannels { ... }` metadata for discovered channel definitions   |
| Workflow files                   | `interface TakoWorkflows { ... }` metadata for discovered workflow definitions |
| Network handlers and schema imports | Typed `call`, `workflow`, and `channel` address registries |
| User vars                        | `process.env` / `import.meta.env` declarations for user-defined variables      |

Secret names are plaintext in [`.tako/secrets.json`](/blog/secrets-without-env-files/) — the values aren't — so `tako generate` emits the type surface without ever touching your encryption key. When you add a secret with `tako secrets set`, `tako.d.ts` picks it up on the next `tako dev`, `tako deploy`, or `tako generate`.

New projects put `tako.d.ts` at the project root. Existing declaration locations, including `src/` and `app/`, are preserved. Keep it included in the TypeScript project. Generated `tako.imports.ts` and convenience `tako.exports.d.ts` live beside it; `app_root` controls primitive discovery, not output placement.

## Share types across apps and languages

Generation also writes `tako-schema.json` beside `tako.toml`. It describes the app's own network handlers, workflows, and channels using JSON Schema and versioned Tako metadata. The `$comment` marks it as generated. A consumer lists local paths or HTTP(S) URLs at the root of its configuration:

```toml
name = "web"
schemas = [
  "../mail/tako-schema.json",
  "https://schemas.example.com/users/v1/tako-schema.json",
]
```

Run `tako gen` in the producers, then in `web`. Local paths resolve from `tako.toml`, so a monorepo can keep the config and source packages in different directories. The generated TypeScript declarations let server code call a declared primitive by string address:

```ts
import { call, channel, workflow } from "tako.sh";

// These names and shapes come from the producer schemas.
const user = await call("users.lookup", { id: "user-123" });
const runId = await workflow("mail.send-email").enqueue({
  to: "reader@example.com", subject: "Welcome",
});
await channel("mail.delivery").publish({
  type: "sent", data: { messageId: "msg-123" },
});
```

`call` returns a direct handler's result without enqueueing. A workflow's `.run(payload)` enqueues and waits for its result; inside another workflow it records the child call durably. Channel publication acknowledges publication, not subscriber processing. The [Network Primitives guide](/docs/network-primitives/) defines these producer examples and their execution behavior.

Go consumers get `tako_imports.go`; Rust consumers get `tako_imports.rs`. These contain native DTOs and address constants, with optional and nullable fields kept distinct. Native output goes in `build.cwd` or the project root. Go DTO and Rust Serde/schemars exporters can produce the same schema format. Native apps run generation explicitly before building, and receiving network calls currently requires the JavaScript dispatcher.

Commit `tako-schema.lock` for remote imports: it embeds the schemas and their hashes, so normal generation works offline. `tako gen --update-schemas` refreshes URLs explicitly. Removing a source removes its imported types. Generation leaves package exports alone and keeps imported contracts out of this app's exported schema.

## Why this is safer than `process.env`

`process.env` is fundamentally a `string → string` map. `process.env.DATBASE_URL` is a valid read; it just returns `undefined`. Your editor can't warn you because the shape of `process.env` isn't tied to your actual secrets.

`tako.secrets.DATBASE_URL` is a compile error. `import foo from "../workflows/bar"` where `bar.ts` doesn't exist is a compile error. If TypeScript sees your file, it'll catch these before they ever run.

A few more guarantees:

- **Redaction by default.** `String(tako.secrets)` returns `"[REDACTED]"`. `JSON.stringify(tako.secrets)` returns `"[REDACTED]"`. Log the whole object by accident and no values leak.
- **Type-only generation.** The generated file has declarations, not runtime code. Runtime state comes from `tako.sh`.
- **Plain ES modules.** Import `tako` from `tako.sh`. Rename-safe, jumps-to-definition, mocks cleanly in tests.

## Try it

`tako generate` runs automatically during [`tako init`](/docs/cli/), [`tako dev`](/docs/development/), [`tako deploy`](/docs/deployment/), and `tako secrets ...`. Most of the time you don't think about it. `tako gen` and `tako g` are aliases. When you want types updated manually:

```bash
tako secrets set STRIPE_KEY --env production
tako generate
# Updates local declarations, network types, and tako-schema.json
```

For Go apps, `tako generate` also emits `tako_secrets.go` with a typed `Secrets` struct. See [the Go SDK post](/blog/the-go-sdk-is-here/) for secret access and [Network Primitives](/docs/network-primitives/) for native schema exporters and generated DTO usage.

Typed runtime config isn't a new idea. Getting it for secrets and runtime state with zero ceremony — no app global, just `tako.sh` plus a generated declaration file — is what `tako generate` is for.
