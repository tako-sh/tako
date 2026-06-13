---
name: tako
description: >-
  Tako CLI commands: init, dev, deploy, secrets, storage, gen, scale, logs,
  rollback, servers, doctor.
type: framework
library: tako.sh
library_version: "0.0.1"
sources:
  - tako-sh/tako:tako/src/cli.rs
---

# Tako CLI

Command-line tool for developing and deploying Tako apps.

## Project Setup

### `tako init`

Initialize a new Tako project. Auto-detects runtime (Go, Bun, Node) from project files (`go.mod`, `package.json`).

```bash
tako init
```

Runs a wizard that prompts for app name, runtime, build preset, entrypoint, assets path, and production route. Creates `tako.toml` and installs the SDK (`go get tako.sh` or `npm install tako.sh`).

### `tako doctor`

Print a local diagnostic report.

```bash
tako doctor
```

## Development

### `tako dev`

Start local development server with built-in HTTPS proxy and `.test` domain.

```bash
tako dev
tako dev --variant staging    # myapp-staging.test
tako dev --tunnel             # start with a temporary public URL
tako dev stop [name]          # stop a running dev app
tako dev list                   # list registered dev apps
```

Features:

- Local HTTPS via auto-generated certificates
- `.test` domain resolution
- Temporary public tunnel URLs via `tako dev --tunnel` or `t` in the interactive UI
- File watching and automatic restart
- Hot reload passthrough for framework dev servers

## Secrets

### `tako secrets set <name> [value] [--env <name>] [--sync]`

Add or update a secret. Prompts for value if omitted. Alias: `add`.

```bash
tako secrets set DATABASE_URL "postgres://..."
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

### `tako secrets key import`

Import a base64url key string. The string includes its id, so import does not take `--env`.

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
- **Runtime types** — augments `tako.sh` with environment names, channel metadata, workflow metadata, and runtime env globals. App runtime values come from `tako.sh`.
- **JS definition stubs** — when `<app_root>/channels/` or `<app_root>/workflows/` already exists, scaffolds `demo.ts` in empty dirs and adds missing default `defineChannel("<file-stem>")` or `defineWorkflow(...)` exports to files that do not have a default export yet. Existing explicit channel names are not rewritten.

Workflow and channel payload types flow from their module types directly (no generated file needed for `.enqueue(payload)` or `.publish({type, data})`).

Re-run after adding/removing secrets, storages, channel files, or workflow files. `tako dev` and `tako deploy` run it automatically.

## Deployment

### `tako deploy [--env <env>] [--yes]`

Build locally and deploy to a Tako server.

```bash
tako deploy
tako deploy --env staging
tako deploy --yes             # skip confirmation
```

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
```

## Servers

### `tako servers add [<host>] [--description <text>]`

Add a deployment server.

### `tako servers list`

List configured servers.

### `tako servers status`

Show status of all servers and deployed apps.

### `tako servers rm [<name>]`

Remove a server.

### `tako servers upgrade [<name>]`

Upgrade Tako on a server.

## CLI Management

### `tako --version`

Show CLI version.

### `tako upgrade`

Upgrade the Tako CLI.

### `tako uninstall [--yes]`

Uninstall Tako and remove all local data.

## Global Flags

| Flag               | Purpose                                           |
| ------------------ | ------------------------------------------------- |
| `--verbose` / `-v` | Verbose output (tracing log lines)                |
| `--ci`             | Non-interactive, deterministic output (no colors) |
| `--dry-run`        | Show what would happen without side effects       |
| `--config` / `-c`  | Use explicit config file instead of `./tako.toml` |
