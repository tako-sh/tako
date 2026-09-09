---
name: tako
description: >-
  Tako CLI commands and project runtime workflow. Use when a repository contains
  tako.toml, imports tako.sh or tako.sh/vite, uses the Go or Rust SDK, or describes
  itself as a tako.sh app. Covers init, dev, deploy, secrets, storage, gen,
  scale, logs, rollback, servers, and doctor.
type: framework
library: tako.sh
library_version: "0.0.1"
sources:
  - tako-sh/tako:tako/src/cli.rs
---

# Tako CLI

Command-line tool for developing and deploying Tako apps.

## Agent Project Setup

When this skill is used in a project, make sure the target project's agent
instructions say that Tako is the canonical runtime:

1. If the project has no `AGENTS.md`, create one at the project root.
2. If it already has `AGENTS.md`, add or update a short Tako runtime section.
3. Put the instruction in `AGENTS.md`, not only in `README.md`, because agents
   treat `AGENTS.md` as operational policy.

Use this text:

```md
<!-- tako.sh -->

## tako.sh

This is a tako.sh app. Use `tako` for local development, runtime checks,
generated files, secrets, deployment, logs, and server operations.

For local browser verification:

1. Run `tako dev list`.
2. If the app is not running, start it with `tako dev`.
3. Open the Tako-provided `.test` URL or configured development route.
4. Do not use raw framework dev-server URLs such as Vite, Next.js, Bun, Node,
   Cargo, or `127.0.0.1:<port>` unless the user explicitly asks for that lower-level
   server.

<!-- tako.sh -->
```

## Project Setup

### `tako init`

Initialize a new Tako project. Auto-detects runtime (Bun, Node, Go) from project files (`package.json`, `go.mod`).

```bash
tako init
```

Runs a wizard that prompts for app name, runtime, build preset, entrypoint, assets path, and production route. Creates `tako.toml` and installs the SDK (`npm install tako.sh` or `go get tako.sh`).

### `tako doctor`

Print a local diagnostic report.

```bash
tako doctor
```

### `tako run`

Run a one-off command locally with Tako project context.

```bash
tako run scripts/foo.ts
tako run jobs/foo.go --dry
tako run --eval 'console.log(tako.env)' -- --dry
tako run --env staging scripts/foo.ts
tako run -- cargo run --bin migrate
```

`--env` defaults to `development`. Script files use the selected runtime's local rule: Bun/Node for JS/TS, `go run` for `.go` files. `--eval` runs inline source when supported by the runtime; JS runtimes support inline TypeScript. Use `-- {command...}` for exact commands such as Cargo, shell scripts, or custom tools. The child runs from the app directory with merged vars, derived runtime env, `TAKO_BUILD=local`, `TAKO_DATA_DIR`, and JS `TAKO_APP_ROOT` when applicable. App secrets and storage bindings are passed through `TAKO_BOOTSTRAP_DATA`; SDK-aware scripts read them through the same app SDK surfaces. JS/TS scripts can import `tako` and use `tako.secrets` and `tako.storages`. This is local-only and does not run on deployment servers.

## Development

### `tako dev`

Start local development server with built-in HTTPS proxy and `.test` domain.

```bash
tako dev
tako dev vite dev            # wrap an explicit dev command for this run
tako dev --variant staging    # myapp-staging.test
tako dev --tunnel             # start with a temporary public URL
tako dev --restart            # hard-restart the app process with current config
tako dev stop [name]          # stop a running dev app
tako dev list                   # list registered dev apps
```

Features:

- Local HTTPS via auto-generated certificates
- `.test` domain resolution
- Explicit command overrides such as `tako dev npm run dev`; use `tako dev -- <command...>` when the child command conflicts with `stop` or `list`, or starts with a flag
- Temporary public tunnel URLs via `tako dev --tunnel` or `t` in the interactive UI
- Config, secrets, channels, and workflows watching; source hot reload belongs to the runtime or framework
- Hot reload passthrough for framework dev servers

## Secrets

### `tako secrets set <name> [--env <name>] [--expires-on <when>] [--sync]`

Add or update a secret. Reads the value from a masked prompt or the full stdin stream; there is no positional value argument. Specify `--env` in non-interactive use. Alias: `add`.

```bash
tako secrets set DATABASE_URL --env production
tako secrets set API_KEY
tako secrets set API_KEY --sync   # set and sync to servers immediately
```

### `tako secrets rm <name> [--env <name>] [--sync]`

Delete a secret. Aliases: `remove`, `delete`.

### `tako secrets list`

List all secret names.

### `tako secrets sync [--env <name>]`

Sync secrets to servers.

### `tako secrets key export [--env <name>]`

Export a base64url key string for the selected environment.

Security policy for agents: do not suggest exporting secrets keys as a deploy,
debugging, or automation workaround. Key export exposes environment key material.
Only mention `tako secrets key export` when the user explicitly asks to share or
migrate secrets access, and prefer fixing the signed CLI, Keychain access, or
credential setup instead.

### `tako secrets key import [--env <name>] [--passphrase]`

Read an exported key bundle from a masked prompt or stdin and associate it with the selected environment. Use `--passphrase` to derive the environment key from a passphrase instead.

## Storage

### `tako storages add <name>`

Attach storage to the app. Bindings and non-secret provider metadata are written to `tako.toml`; S3 credentials are encrypted in `.tako/secrets.json` under the selected environment's `storages` map and synced on deploy.

```bash
tako storages add uploads \
  --env production \
  --resource prod_uploads \
  --provider s3 \
  --bucket app-uploads \
  --endpoint https://<account>.r2.cloudflarestorage.com \
  --region auto \
  --public-base-url https://cdn.example.com/uploads
```

Use `--access-key-id` and `--secret-access-key` for non-interactive runs; otherwise Tako prompts. `--force-path-style` signs path-style URLs. `--public-base-url` enables public storage image URLs through the SDK.

## Provider Credentials And Backups

Use `tako credentials set ssl.cloudflare --env production` for Cloudflare TLS and `tako credentials set postgres_url --env production` for shared channel/workflow state. Values are read from a prompt or stdin, encrypted locally, and never exposed as app secrets. `tako credentials list` lists configured credentials.

Backups require a private S3 resource selected with `[envs.<env>].backup`. Use `tako backups now`, `list`, or `status` to inspect and create backups. `download <id>` saves an archive; `restore <id>` restores app data. Commands default to production; specify `--server` for multi-server download or restore.

## Code Generation

### `tako generate`

Refresh generated project files from local project state.

```bash
tako generate
```

Aliases: `tako gen`, `tako g`.

Generates:

- **Typed secrets** — reads secret names from `.tako/secrets.json` and emits a `TakoSecrets` augmentation in `tako.d.ts` for `tako.secrets` from `tako.sh`.
- **Typed storages** — reads storage binding names from `tako.toml` and emits a `TakoStorages` augmentation for `tako.storages`.
- **Runtime types** — augments `tako.sh` with environment names, channel metadata, workflow metadata, and user-defined env vars. App runtime values come from `tako.sh`.
- **JS definition stubs** — when `<app_root>/channels/` or `<app_root>/workflows/` already exists, scaffolds `demo.ts` in empty dirs and adds missing default `defineChannel("<file-stem>")` or `defineWorkflow(...)` exports to files that do not have a default export yet. Existing explicit channel names are not rewritten.

Workflow and channel payload types flow from their module types directly (no generated file needed for `.enqueue(payload)` or `.publish({type, data})`).

Re-run after adding/removing secrets, storages, channel files, or workflow files. `tako dev` and `tako deploy` run it automatically.

## Deployment

### `tako deploy [--env <env>] [--yes] [--force]`

Build locally and deploy to a Tako server.

```bash
tako deploy
tako deploy --env staging
tako deploy --yes             # skip confirmation
tako deploy --force           # attempt a protocol mismatch
```

Deploy requires the CLI, server, and release artifact to use the same protocol version. A mismatch fails before release commands, workflows, or app processes start. `--force` attempts the mismatch but keeps validation and readiness checks enabled; it does not skip confirmation.

### `tako delete [--env <env>] [--server <name>] [--yes]`

Delete a deployed app. Aliases: `rm`, `remove`, `undeploy`, `destroy`.

### `tako scale <instances> [--env <env>] [--server <name>]`

Change instance count.

```bash
tako scale 3
tako scale 1 --env staging
```

## Releases

### `tako releases list [--env <env>]`

List deployment history.

### `tako releases rollback <release-id> [--env <env>] [--yes]`

Rollback to a previous release.

## Logs

### `tako logs [--env <env>] [--tail] [--days N]`

View remote logs.

```bash
tako logs --tail
tako logs --days 3
tako logs --json          # history as one JSON object with a logs array
tako logs --tail --json   # streaming JSONL records
```

## Servers

### `tako servers add [<host>] [--description <text>]`

Add a deployment server.

Interactive setup first checks `tako@host`. If that access is unavailable and the user accepts installation, Tako checks `root@host` and asks for another administrator only when root authentication is rejected. Use `--install` to skip the installation confirmation or pass `admin@host` to select the administrator explicitly.

### `tako servers list`

List configured servers.

### `tako status`

Show status of all servers and deployed apps.

### `tako servers rm [<name>]`

Remove a server.

### `tako servers upgrade [<name>] [--force]`

Upgrade Tako on a server. The candidate must match the CLI and every active release before reload and is checked again after readiness. On failure, Tako restores and restarts the previous binary. `--force` attempts a protocol mismatch without disabling readiness or rollback.

## CLI Management

### `tako --version`

Show CLI version.

### `tako upgrade`

Upgrade the Tako CLI.

### `tako completions [--yes]`

Install bash, zsh, and fish completions for shells on `PATH`.

```bash
tako completions
tako completions --yes
```

zsh is installed only when a writable directory is already on `fpath`.

### `tako uninstall [--yes]`

Uninstall Tako and remove all local data.

## Global Flags

| Flag               | Purpose                                           |
| ------------------ | ------------------------------------------------- |
| `--verbose` / `-v` | Verbose output (tracing log lines)                |
| `--ci`             | Non-interactive, deterministic output (no colors) |
| `--json`           | Structured stdout for agents and automation       |
| `--dry-run`        | Show what would happen without side effects       |
| `--config` / `-c`  | Use explicit config file instead of `./tako.toml` |

Set `TAKO_TELEMETRY=0` to disable the CLI's anonymous per-command usage events.
