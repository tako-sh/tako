---
name: tako
description: >-
  Tako CLI commands: init, dev, deploy, secrets, typegen, scale, logs,
  rollback, servers, doctor. Includes output design patterns for interactive
  and plain modes.
type: framework
library: tako.sh
library_version: "0.0.1"
sources:
  - lilienblum/tako:tako/src/cli.rs
---

# Tako CLI

Command-line tool for developing and deploying Tako apps.

## Project Setup

### `tako init`

Initialize a new Tako project. Auto-detects runtime (Go, Bun, Node, Deno) from project files (`go.mod`, `package.json`, `deno.json`).

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
tako dev stop [name]          # stop a running dev app
tako dev ls                   # list registered dev apps
```

Features:

- Local HTTPS via auto-generated certificates
- `.test` domain resolution
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

### `tako secrets ls`

List all secret names.

### `tako secrets sync [--env <name>]`

Sync secrets to servers.

### `tako secrets key derive [--env <name>]`

Derive a key from a passphrase.

### `tako secrets key export [--env <name>]`

Export the encryption key.

## Code Generation

### `tako typegen`

Generate typed accessors from local project files. Outputs to `tako.gen.ts`.

```bash
tako typegen
```

Generates:

- **Typed secrets** — reads secret names from `.tako/secrets.json`, emits a `Secrets` interface and `secrets` export from `tako.gen.ts`.
- **Runtime accessors** — emits `env`, `isDev`, `isProd`, `port`, `host`, `build`, `dataDir`, `appDir`, `logger` exports that wrap process state and the fd-3 bootstrap.
- **JS definition stubs** — when `channels/` or `workflows/` already exists, scaffolds `demo.ts` in empty dirs and adds missing default `defineChannel({ name: "<file-stem>" })` / `defineWorkflow(...)` exports to files that do not have a default export yet. Existing explicit channel names are not rewritten.

Workflow and channel payload types flow from their module types directly (no typegen needed for `.enqueue(payload)` or `.publish({type, data})`).

Re-run after adding/removing secrets or workflow files. `tako dev` and `tako deploy` run it automatically.

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

### `tako releases ls [--env <env>]`

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

### `tako servers ls`

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

### `tako implode [--yes]`

Uninstall Tako and remove all local data.

## Global Flags

| Flag               | Purpose                                           |
| ------------------ | ------------------------------------------------- |
| `--verbose` / `-v` | Verbose output (tracing log lines)                |
| `--ci`             | Non-interactive, deterministic output (no colors) |
| `--dry-run`        | Show what would happen without side effects       |
| `--config` / `-c`  | Use explicit config file instead of `./tako.toml` |

## Output Design

### Two modes

- **Interactive** (`is_pretty() && is_interactive()`): spinners, colors, diamond prompts, padding.
- **Plain** (`--verbose` or `--ci`): tracing log lines, no colors, no spinners.

### Interactive padding

In interactive mode, plain text lines (`info`, `muted`, `hint`, `section`, `heading`) are indented 2 spaces so they align with symbol-prefixed lines (`✔`/`✘`/`⠋` already occupy 2 chars).

### Elapsed times

No parentheses: `3s`, `1m10s`, `3s, 72 MB` (comma-separated when combined with size).

### Prompts — diamond style

```
◆ App name                   ← accent filled diamond + accent label (active)
› myapp_                     ← accent chevron on the input line
  Hint text here             ← optional muted hint under the input

◇ App name                   ← completed: muted outlined diamond + muted label
› myapp                      ← completed: muted chevron stays with the value
```

- Active: `◆` filled diamond, accent color label, accent `›` on the input line.
- Completed (inactive): `◇` outlined diamond, muted label, no border, no hint.
- `select()` and `confirm()` use the same diamond style for their summary lines.

### Spinners

```
⠋ Building…  3s              ← PhaseSpinner (major operation)
✔ Build complete  5s         ← success (double space before time)
```

### Grouped spinner

```
⠋ Building services  10s
  ✔ server1  7s
  ⠋ server2  3s
  ·  server3               ← pending, muted
```

Use `GroupedSpinner::new(parent, &["server1", "server2", "server3"])`.

### Step flow (linear phases)

```
⠋ Pushing images  3s
·  Applying migrations       ← pre-rendered pending, muted (pretty only)
·  Health checks
```

Use `StepFlow::new(&["Pushing images", "Applying migrations", "Health checks"])`.
Call `advance()` to complete each step and `finish()` when done.

### Progress bar

Single line with elapsed time first, then block bar, percentage, and transferred amount:

```
⠋ Uploading…  42s  ████████████░░░░  72%  (84 KB/116 MB)
✔ Uploaded  42s, 116 MB
```

### Error block

Red left border + fixed-width dimmed-red background, capped at 72 chars:

```
│ Cannot find module './queue-handler' Did you mean './queue-manager'?
```

Use `output::error_block(message)` for inline/validation errors.
Use `output::error(message)` for the standard `✘ message` format.
