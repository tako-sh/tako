---
title: "Secrets Without .env Files"
date: "2026-04-06T11:39"
description: "How Tako encrypts secrets at rest, injects them via fd 3 at runtime, and generates typed accessors — so plaintext never touches disk."
image: 8f2cb3d41b80
---

Every deploy tool has a secrets story. Most of them end with "add it to your `.env` file." The `.env` file sits in `.gitignore`, gets copy-pasted between teammates over Slack, and lives as plaintext on every server it touches. If someone commits it by accident — and someone always does — you're rotating every key in the file.

Tako does secrets differently. Encrypted at rest, injected at runtime through a file descriptor, and typed so your editor knows what's available. No plaintext touches disk. No environment variables leak to child processes.

## The problem with `.env`

The `.env` convention started as a convenience and became load-bearing infrastructure. Here's what you're actually trusting when you use one:

| Risk                      | What happens                                                                         |
| ------------------------- | ------------------------------------------------------------------------------------ |
| **Plaintext on disk**     | Anyone with server access reads your secrets                                         |
| **Environment variables** | Inherited by child processes, visible in `/proc/<pid>/environ`                       |
| **Manual distribution**   | Copy-paste via Slack, email, or shared drives                                        |
| **No encryption**         | Accidentally committed = fully exposed                                               |
| **No types**              | Typo in `process.env.DATBASE_URL` fails silently at runtime                          |
| **No audit trail**        | No way to know which secrets exist in which environments                             |
| **Agent-readable**        | AI coding agents can read `.env` files — one prompt injection away from exfiltration |

Some tools improve on this by integrating with external vaults — 1Password, AWS Secrets Manager, Doppler. That works, but it's another service to configure, pay for, and debug when deploys fail at 2 AM.

## How Tako handles secrets

### Encrypted at rest

When you run [`tako secrets set`](/docs/cli), Tako encrypts the value with **AES-256-GCM** before writing it to `.tako/secrets.json`. The first secret set for an environment creates a random local key, cached under Tako's data directory.

```bash
$ tako secrets set DATABASE_URL --env production
Enter value: ****
  Set secret DATABASE_URL for environment production
```

The resulting file is safe to commit. It contains only encrypted blobs and a per-environment key id:

```json
{
  "production": {
    "key_id": "0123456789abcdef",
    "secrets": {
      "DATABASE_URL": "base64(nonce + ciphertext + GCM tag)",
      "STRIPE_KEY": "base64(nonce + ciphertext + GCM tag)"
    }
  }
}
```

Secret names are visible (so you can list what exists without decrypting), but values are useless without the matching local key. Each environment gets its own key id, so production and staging can be shared independently.

### Team sharing without a vault

No external service required. When a new team member joins:

1. They pull the repo (which includes `.tako/secrets.json`)
2. A teammate runs `tako secrets key export --env production`
3. They send the single exported key string out of band
4. The new team member runs `tako secrets key import`
5. The key is cached locally at `$TAKO_HOME/keys/{key_id}` with `0600` permissions

Share only the environment keys people need. For CI, import the exported key bundle into the runner's Tako data directory before decrypting or syncing secrets.

### fd 3 injection — secrets never hit disk on the server

This is the part that matters most. When `tako-server` spawns your app, it doesn't set environment variables. Instead, it opens **file descriptor 3** as a pipe and writes the decrypted secrets as JSON before your code starts.

```d2
direction: right

server: tako-server {
  style.font-size: 13
}

pipe: fd 3 pipe {
  shape: circle
  style.font-size: 13
}

app: Your App {
  shape: hexagon
}

server -> pipe: "write JSON"
pipe -> app: "read + close"
```

Your app reads fd 3 once at startup, parses the JSON, and the pipe is closed. The secrets exist only in process memory — never written to disk on the server, never in environment variables, never visible in `/proc/<pid>/environ` or `ps auxe`.

The [Tako SDK](/docs) handles this automatically. In JavaScript, `tako typegen` emits a project-local `tako.gen.ts` that exports a typed `secrets` bag. Your app imports what it needs:

```typescript
// tako.gen.ts is populated from fd 3 before your code runs
import { secrets } from "../tako.gen";

const db = secrets.DATABASE_URL;

console.log(secrets); // "[REDACTED]"
JSON.stringify(secrets); // "[REDACTED]"
```

The SDK wraps secrets in a Proxy that redacts on `toString()` and `toJSON()` — so accidental logging never leaks values. In Go, it's the same idea with thread-safe accessors.

### Typed secrets with `tako typegen`

Run [`tako typegen`](/docs/cli) and Tako reads your encrypted secrets file to generate type definitions — without decrypting the values (remember, names are plaintext).

**TypeScript** gets a `tako.gen.ts` that exports a typed `Secrets` interface and a `secrets` instance:

```typescript
export interface Secrets {
  readonly DATABASE_URL: string;
  readonly STRIPE_KEY: string;
  toString(): "[REDACTED]";
  toJSON(): "[REDACTED]";
}
```

**Go** gets a `tako_secrets.go` with PascalCase accessors:

```go
var Secrets = struct {
  DatabaseUrl func() string
  StripeKey   func() string
}{...}
```

Autocomplete in your editor. Compile-time errors for typos. No more `process.env.DATBASE_URL` bugs discovered in production.

## What about rotation?

Change a secret locally, then sync it to your servers:

```bash
tako secrets set STRIPE_KEY --env production
tako secrets sync --env production
```

`tako secrets sync` decrypts locally, pushes to the server over SSH, and triggers a rolling restart. New instances get the updated values via fd 3. The old instances keep running with old values until they're drained — zero downtime, same as a regular deploy.

## The full picture

|                         | **.env files**           | **External vault**    | **Tako secrets**        |
| ----------------------- | ------------------------ | --------------------- | ----------------------- |
| **Encryption at rest**  | None                     | Vault-side            | AES-256-GCM, local      |
| **Storage**             | Plaintext, gitignored    | External service      | Encrypted, committed    |
| **Distribution**        | Manual copy-paste        | API calls at deploy   | Passphrase-derived keys |
| **Runtime injection**   | Environment variables    | Environment variables | fd 3 pipe               |
| **Type safety**         | None                     | None                  | Generated types         |
| **Leak surface**        | Disk, env, logs, `/proc` | Env, logs, `/proc`    | Process memory only     |
| **External dependency** | None                     | Vault service         | None                    |

## Try it

```bash
tako secrets set API_KEY --env production
tako secrets set API_KEY --env development
tako secrets ls
```

Check the [CLI reference](/docs/cli) for the full command set, or the [deployment docs](/docs/deployment) for how secrets flow during a deploy. The [development guide](/docs/development) covers how secrets work locally with `tako dev`.

Your secrets deserve better than a plaintext file in `.gitignore`.
