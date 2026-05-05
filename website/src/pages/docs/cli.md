---
layout: ../../layouts/DocsLayout.astro
title: "Tako CLI reference for local development and self-hosted deploys - Tako Docs"
heading: CLI Reference
current: cli
description: "Complete CLI reference for Tako commands including init, dev, deploy, servers, secrets, status, logs, and global flags."
---

# CLI Reference

```bash
tako [--version] [-v|--verbose] [--ci] [--dry-run] [-c|--config <CONFIG>] <command>
```

## Global Options

| Flag                      | Description                                                                     |
| ------------------------- | ------------------------------------------------------------------------------- |
| `--version`               | Print version and exit.                                                         |
| `-v`, `--verbose`         | Show an append-only transcript with timestamps and debug detail.                |
| `--ci`                    | Deterministic non-interactive output: no color, spinners, or prompts.           |
| `--dry-run`               | Show planned side effects without performing them where supported.              |
| `-c`, `--config <CONFIG>` | Select an app config file instead of `./tako.toml`; `.toml` suffix is optional. |

App-scoped commands use the selected config file's parent directory as the app directory.

## Installation

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

Upgrade the local CLI:

```bash
tako upgrade
```

GitHub-backed update checks and release downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.

## `tako init`

Create a project config and install the SDK.

```bash
tako init
tako init -c staging
```

`init` detects the runtime, offers preset choices, writes `tako.toml`, updates `.gitignore`, pins the local runtime version when possible, and installs `tako.sh` through the selected package manager.

If the config file already exists, interactive terminals ask before overwriting. Non-interactive runs leave it untouched.

## `tako dev`

Start or attach to a local development session.

```bash
tako dev
tako dev --variant foo
tako dev --var foo
```

`--variant` changes the local hostname from `{app}.test` to `{app}-{variant}.test`.

Interactive shortcuts:

| Key      | Action                                                        |
| -------- | ------------------------------------------------------------- |
| `r`      | Restart the app process.                                      |
| `l`      | Toggle LAN mode with `.local` aliases for managed dev routes. |
| `b`      | Background the app and exit the CLI.                          |
| `Ctrl-C` | Stop the app and unregister routes.                           |

Subcommands:

```bash
tako dev stop
tako dev stop dashboard
tako dev stop --all
tako dev ls
tako dev list
```

## `tako doctor`

Print a local diagnostic report.

```bash
tako doctor
```

The report covers the dev daemon, local DNS, loopback setup, macOS dev proxy state, and port reachability. A missing dev daemon is reported as `not running` and exits successfully.

## `tako deploy`

Build and deploy an app to one environment.

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

`--env` defaults to `production`. Production deploys require confirmation unless `--yes` / `-y` is provided.

Deploy validates config, builds locally, uploads artifacts, prepares releases on each server, runs the release command on the leader when configured, and performs rolling update.

## `tako logs`

View remote app logs and related server diagnostics.

```bash
tako logs
tako logs --env staging
tako logs --days 7
tako logs --tail
tako logs --json
```

`--env` defaults to `production`. `--days` defaults to `3` and applies to timestamped app log-file lines and server journal diagnostics. `--tail` streams continuously and conflicts with `--days`.

Logs include app stdout/stderr plus `tako-server` lifecycle, health, and proxy diagnostics for the app's deployed routes. JS/TS production HTTP entrypoints route `console.*`, uncaught exceptions, and unhandled rejections into the same app log stream. Remote fetch/connect failures are reported instead of being shown as empty logs.

`--json` emits compact JSONL for agents and automation. Each stdout line is one log event with stable short keys.

## `tako releases`

List or roll back deployed releases.

```bash
tako releases ls
tako releases ls --env staging
tako releases rollback abc1234
tako releases rollback abc1234 --env staging --yes
```

`ls` shows newest releases first and marks the current release. `rollback` performs the standard rolling-update flow using the selected release.

## `tako scale`

Change desired instance count.

```bash
tako scale <instances> --env <ENV>
tako scale <instances> --server <SERVER>
tako scale <instances> --app <APP>/<ENV> --server <SERVER>
```

Examples:

```bash
tako scale 2 --env production
tako scale 0 --env production
tako scale 1 --server la
tako scale 3 --app dashboard/production --server la
```

When `--server` is omitted, `--env` is required and Tako scales every server listed in that environment. In a project directory, `--server` without `--env` defaults to `production`. Outside a project directory, pass `--app`.

## `tako delete`

Delete one deployed app target.

```bash
tako delete
tako delete --env production --server la
tako delete --env staging --server staging --yes
```

Aliases:

```bash
tako rm
tako remove
tako undeploy
tako destroy
```

Interactive mode can discover targets and prompt. Non-interactive mode requires `--yes`, `--env`, and `--server`.

## `tako secrets`

Manage encrypted project secrets.

```bash
tako secrets set DATABASE_URL
tako secrets set DATABASE_URL --env staging
tako secrets set API_KEY --sync
tako secrets rm API_KEY
tako secrets rm API_KEY --env staging --sync
tako secrets ls
tako secrets sync
tako secrets sync --env production
```

Aliases:

- `set`: `add`
- `rm`: `remove`, `delete`, `del`
- `ls`: `list`, `show`

Secret values are read from an interactive password prompt or stdin. `sync` sends local encrypted secrets to mapped servers after decrypting them locally.

### Secret Keys

```bash
tako secrets key derive
tako secrets key derive --env staging
tako secrets key export
tako secrets key export --env staging
```

`derive` writes the environment's cached key under Tako's data directory at `keys/{sha256(salt)[:16]}`. `export` reads that key and copies it to the clipboard.

## `tako servers`

Manage global server inventory.

```bash
tako servers add
tako servers add 203.0.113.10 --name la
tako servers add 203.0.113.10 --name la --description "Los Angeles" --port 22
tako servers add 203.0.113.10 --name la --no-test
```

Without `host`, `add` opens an interactive setup wizard. With `host`, `--name` is required. SSH checks detect and store target metadata (`arch`, `libc`) unless `--no-test` is used.

Other server commands:

```bash
tako servers ls
tako servers list
tako servers rm la
tako servers remove la
tako servers delete la
tako servers status
tako servers restart la
tako servers restart la --force
tako servers upgrade
tako servers upgrade la
tako servers setup-wildcard
tako servers setup-wildcard --env production
tako servers implode la
tako servers implode la --yes
```

GitHub-backed server upgrade metadata and archive downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.

`servers status` reads all configured servers and prints a snapshot of deployed apps. It can run from any directory.

## `tako typegen`

Generate typed runtime and secret accessors.

```bash
tako typegen
```

JavaScript/TypeScript projects get `tako.gen.ts`. Go projects get `tako_secrets.go`.

## `tako version`

Print version information.

```bash
tako version
tako --version
```

Rolling builds render as the package version plus source commit suffix when available.

## `tako implode`

Remove local Tako CLI data and installation.

```bash
tako implode
tako implode --yes
tako uninstall --yes
```

This is destructive for local Tako state.
