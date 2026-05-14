---
layout: ../../layouts/DocsLayout.astro
title: "Tako CLI reference for local development and self-hosted deploys - Tako Docs"
heading: CLI Reference
current: cli
description: "Complete CLI reference for Tako commands including init, dev, deploy, servers, secrets, storage, status, logs, and global flags."
---

# CLI Reference

```bash
tako [--version] [-v|--verbose] [--ci] [--dry-run] [-c|--config <CONFIG>] [--ssh-passphrase <PASSPHRASE>] <command>
```

## Global Options

| Flag                            | Description                                                                                          |
| ------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `--version`                     | Print the CLI version and exit.                                                                      |
| `-v`, `--verbose`               | Show an append-only transcript with debug detail.                                                    |
| `--ci`                          | Deterministic non-interactive output: no colors, spinners, or prompts.                               |
| `--dry-run`                     | Show side effects without performing them. Supported by deploy, server add/remove, and delete flows. |
| `-c`, `--config <CONFIG>`       | Select an app config file instead of `./tako.toml`; `.toml` suffix is optional.                      |
| `--ssh-passphrase <PASSPHRASE>` | Passphrase for encrypted local SSH private keys.                                                     |

Status, progress, prompts, and logs go to stderr. Command results and machine-readable data go to stdout.

App-scoped commands treat the selected config file's parent directory as the app directory. That includes `init`, `dev`, `logs`, `deploy`, `releases`, `delete`, `secrets`, `storage`, `generate`, and project-context `scale`.

## `tako init`

```bash
tako init
tako init -c staging
```

Creates a `tako.toml` template, updates `.gitignore`, prompts for app name and production route, detects runtime, optionally prompts for JS `app_root`, and installs the SDK through the selected runtime package manager.

If the selected config file exists, interactive mode asks before overwriting. Non-interactive mode leaves it untouched and prints `Operation cancelled`.

## `tako generate`

Aliases: `tako gen`, `tako g`.

```bash
tako generate
tako gen -c apps/web/tako.production
```

Refreshes generated files for the current project:

- JS/TS: `tako.d.ts` with typed runtime metadata, environment names, secrets, storages, channels, and workflow metadata.
- Go: `tako_secrets.go` with typed secret accessors.

For JS/TS projects, generation keeps an existing `tako.d.ts` location in `app/`, `src/`, or the project root. It removes legacy `tako.gen.ts` on regeneration. If `<app_root>/channels/` or `<app_root>/workflows/` exists, it can scaffold missing default exports and `demo.ts` files in empty directories.

## `tako dev`

```bash
tako dev
tako dev --variant preview
tako dev --var preview
tako dev stop
tako dev stop my-app
tako dev stop --all
tako dev list
```

Starts or attaches to a local HTTPS dev session.

Options and aliases:

| Command or flag    | Meaning                                          |
| ------------------ | ------------------------------------------------ |
| `--variant <name>` | Run a DNS variant such as `my-app-preview.test`. |
| `--var <name>`     | Alias for `--variant`.                           |
| `dev stop [name]`  | Stop one registered dev app.                     |
| `dev stop --all`   | Stop all registered dev apps.                    |
| `dev list`         | List registered dev apps.                        |
| `dev ls`           | Alias for `dev list`.                            |

Interactive shortcuts:

| Key      | Action                                      |
| -------- | ------------------------------------------- |
| `r`      | Restart the app process.                    |
| `l`      | Toggle LAN mode.                            |
| `b`      | Background the app and leave routes active. |
| `Ctrl-C` | Stop and unregister the app.                |

## `tako doctor`

```bash
tako doctor
```

Prints a local diagnostic report and exits. It reports dev daemon state, local DNS status, and platform-specific proxy or redirect setup. If the dev daemon is not running, doctor exits successfully with a hint to start `tako dev`.

## `tako deploy`

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

Options:

| Flag          | Meaning                                          |
| ------------- | ------------------------------------------------ |
| `--env <ENV>` | Environment to deploy. Defaults to `production`. |
| `-y`, `--yes` | Skip confirmation prompts.                       |

Deploy validates config, resolves runtime and preset metadata, builds locally, uploads artifacts, runs production install on each server, optionally runs the release command on the leader, and rolls instances forward.

`development` is reserved for `tako dev` and cannot be deployed.

Interactive production deploys ask for confirmation only when the environment is implicit. Passing `--env production` or `--yes` skips it.

## `tako logs`

```bash
tako logs
tako logs --env staging
tako logs --days 7
tako logs --tail
tako logs --json
```

Options:

| Flag          | Meaning                                        |
| ------------- | ---------------------------------------------- |
| `--env <ENV>` | Environment to read. Defaults to `production`. |
| `--days <N>`  | History window in days. Default: `3`.          |
| `--tail`      | Stream continuously. Conflicts with `--days`.  |
| `--json`      | Emit compact JSONL for agents and automation.  |

Human logs are formatted with timestamp, level, source, and message columns. With multiple servers, each line is prefixed by server name. History mode uses a pager in interactive terminals.

## `tako releases`

```bash
tako releases list
tako releases list --env staging
tako releases rollback abc1234
tako releases rollback abc1234 --env production --yes
```

Subcommands:

| Command                              | Meaning                                   |
| ------------------------------------ | ----------------------------------------- |
| `releases list [--env <ENV>]`        | List release/build history, newest first. |
| `releases ls [--env <ENV>]`          | Alias for `releases list`.                |
| `releases rollback <ID> [--env ENV]` | Roll back using the normal rolling flow.  |

Rollback to implicit production asks for confirmation unless `--yes` is provided.

## `tako scale`

```bash
tako scale 2 --env production
tako scale 0 --env production
tako scale 1 --server la
tako scale 1 --server la --app dashboard/production
```

Options:

| Argument or flag  | Meaning                                                         |
| ----------------- | --------------------------------------------------------------- |
| `<instances>`     | Desired instance count per targeted server.                     |
| `--env <ENV>`     | Environment to scale.                                           |
| `--server <NAME>` | Specific server to scale.                                       |
| `--app <APP>`     | App name or `{app}/{env}` id, required outside project context. |

Scale settings persist on the server across restarts, deploys, and rollbacks. `0` enables scale-to-zero.

## `tako delete`

Aliases: `tako rm`, `tako remove`, `tako undeploy`, `tako destroy`.

```bash
tako delete --env production --server la
tako delete --env production --server la --yes
```

Options:

| Flag              | Meaning                               |
| ----------------- | ------------------------------------- |
| `--env <ENV>`     | Environment to delete from.           |
| `--server <NAME>` | Specific server deployment to delete. |
| `-y`, `--yes`     | Skip confirmation prompts.            |

Delete removes exactly one environment/server deployment target. In non-interactive mode, `--yes`, `--env`, and `--server` are required.

## `tako servers`

### `tako servers add`

```bash
tako servers add host.example.com --name la
tako servers add ubuntu@host.example.com --install --name la
```

Options:

| Flag                   | Meaning                                                     |
| ---------------------- | ----------------------------------------------------------- |
| `--name <NAME>`        | Server name. Defaults to host's first DNS label when valid. |
| `--description <TEXT>` | Optional metadata shown by `servers list`.                  |
| `--port <PORT>`        | SSH port. Default: `22`.                                    |
| `--http-port <PORT>`   | Public HTTP port used when installing `tako-server`.        |
| `--https-port <PORT>`  | Public HTTPS port used when installing `tako-server`.       |
| `--install`            | Install or repair `tako-server` before adding.              |
| `--admin-user <USER>`  | Admin SSH user for `--install`.                             |

`admin-user@host` is shorthand for choosing the admin user and enabling install/repair when needed.

If the host was bootstrapped with `install-server.sh`, `tako servers add` configures and starts the stopped service before saving the server.

### Other Server Commands

```bash
tako servers rm la
tako servers list
tako servers status
tako servers reload la
tako servers reload la --force
tako servers upgrade
tako servers upgrade la
tako servers configure <name>
tako servers uninstall la
```

| Command                         | Meaning                                                                             |
| ------------------------------- | ----------------------------------------------------------------------------------- |
| `servers rm [name]`             | Remove a server from global config. Aliases: `remove`, `delete`.                    |
| `servers list`                  | List configured servers. Alias: `ls`.                                               |
| `servers status`                | Show deployment status across configured servers. Alias: `info`.                    |
| `servers reload <name>`         | Reload `tako-server` without downtime by default.                                   |
| `servers reload <name> --force` | Full service restart, which may cause brief downtime.                               |
| `servers upgrade [name]`        | Upgrade one server or all servers with graceful reload and rollback.                |
| `servers configure <name>`      | Configure server settings: DNS-01 wildcard certificates or trusted proxy source IP. |
| `servers uninstall [name]`      | Remove `tako-server` and all data from a remote server.                             |

## `tako secrets`

```bash
tako secrets set DATABASE_URL --env production
tako secrets set API_KEY --env production --sync
tako secrets rm API_KEY --env production --sync
tako secrets list
tako secrets sync --env production
```

Subcommands:

| Command                                 | Meaning                                                  |
| --------------------------------------- | -------------------------------------------------------- |
| `secrets set [--env ENV] [--sync] NAME` | Set or update one secret. Alias: `add`.                  |
| `secrets rm [--env ENV] [--sync] NAME`  | Remove one secret. Aliases: `remove`, `delete`, `del`.   |
| `secrets list`                          | List secret names by environment. Aliases: `ls`, `show`. |
| `secrets sync [--env ENV]`              | Sync local encrypted secrets to target servers.          |

Interactive `set` prompts for the value with masked input. Non-interactive `set` reads one line from stdin.

### Secret Keys

```bash
tako secrets key export --env production
tako secrets key import --env production
tako secrets key import --passphrase --env production
```

| Command                                     | Meaning                                             |
| ------------------------------------------- | --------------------------------------------------- |
| `secrets key export [--env ENV]`            | Copy a self-contained key bundle.                   |
| `secrets key import [--env ENV]`            | Import an exported key bundle from prompt or stdin. |
| `secrets key import --passphrase --env ENV` | Import a passphrase-derived environment key.        |

## `tako storages`

Attach S3-compatible object storage to the current app:

```bash
tako storages add uploads \
  --env production \
  --provider r2 \
  --bucket app-uploads \
  --endpoint https://<account>.r2.cloudflarestorage.com \
  --region auto \
  --public-base-url https://cdn.example.com/uploads
```

| Option                | Meaning                                                       |
| --------------------- | ------------------------------------------------------------- |
| `--env ENV`           | Environment to attach. Defaults to `production`.              |
| `--provider s3\|r2`   | Storage provider. Defaults to `s3`.                           |
| `--bucket NAME`       | Bucket name. Required.                                        |
| `--endpoint URL`      | HTTPS S3-compatible endpoint. Required.                       |
| `--region REGION`     | Signing region. Defaults to `auto`.                           |
| `--access-key-id KEY` | Access key id. Prompts when omitted in interactive terminals. |
| `--secret-access-key` | Secret access key. Prompts when omitted in interactive runs.  |
| `--force-path-style`  | Sign path-style URLs instead of virtual-hosted bucket URLs.   |
| `--public-base-url`   | Public base URL used by `public: true` SDK helpers.           |

The command writes encrypted credentials to `.tako/storages.json`. Deploy syncs storage bindings with the app release; there is no separate storage sync command.

## `tako upgrade`

```bash
tako upgrade
```

Upgrades the local CLI installation. Homebrew installs use `brew upgrade tako`; other installs download the hosted CLI archive. Downloaded archives require a valid SHA-256 checksum before extraction.

## `tako uninstall`

```bash
tako uninstall
tako uninstall --yes
```

Removes the local Tako CLI, local data directories, and platform-specific dev services/config installed by `tako dev`. System-level cleanup may require sudo.

## `tako version`

```bash
tako version
tako --version
```

Prints the CLI version. Rolling builds use `<base>-<sha7>`.

## `tako help`

```bash
tako help
tako
```

Shows available commands and brief descriptions.
