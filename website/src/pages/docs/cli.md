---
layout: ../../layouts/DocsLayout.astro
title: "Tako CLI reference for local development and self-hosted deploys - Tako Docs"
heading: CLI Reference
current: cli
description: "Complete CLI reference for Tako commands including init, dev, deploy, servers, secrets, status, logs, and global flags."
---

# CLI Reference

```bash
tako [--version] [-v|--verbose] [--ci] [--dry-run] [-c|--config <CONFIG>] [--ssh-passphrase <PASSPHRASE>] <command>
```

## Global Options

| Flag                            | Description                                                                                                  |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `--version`                     | Print the CLI version and exit.                                                                              |
| `-v`, `--verbose`               | Show an append-only transcript with debug detail.                                                            |
| `--ci`                          | Use deterministic non-interactive output: no colors, spinners, or prompts.                                   |
| `--dry-run`                     | Show planned side effects without performing them. Supported by deploy, server add/remove, and delete flows. |
| `-c`, `--config <CONFIG>`       | Select an app config file instead of `./tako.toml`; `.toml` suffix is optional.                              |
| `--ssh-passphrase <PASSPHRASE>` | Passphrase for encrypted local SSH private keys.                                                             |

Progress, logs, and prompts go to stderr. Machine-readable results and command output go to stdout.

App-scoped commands use the selected config file's parent directory as the app directory.

## `tako init`

```bash
tako init
tako init -c staging
```

Creates a `tako.toml` template, updates `.gitignore`, prompts for app name and production route, detects runtime, and installs the `tako.sh` SDK with the selected runtime's package manager.

For JS projects, init prompts for `app_root`; the generated config omits it when the selected root is the default `src`.

If the config file already exists, interactive mode asks before overwriting. Non-interactive mode leaves the file untouched and exits with `Operation cancelled`.

## `tako dev`

```bash
tako dev
tako dev --variant preview
tako dev stop
tako dev stop my-app
tako dev stop --all
tako dev ls
```

Starts or attaches to a local HTTPS dev session.

Options and aliases:

- `--variant`, `--var`: run a DNS variant such as `my-app-preview.test`
- `stop`: stop one registered app
- `stop --all`: stop all registered apps
- `ls`, `list`: list registered dev apps

Interactive shortcuts:

- `r`: restart app process
- `l`: toggle LAN mode
- `b`: background the app and leave routes active
- `Ctrl-C`: stop and unregister the app

## `tako doctor`

```bash
tako doctor
```

Prints a local diagnostic report for installed binaries, local dev prerequisites, DNS/proxy setup, and platform state.

## `tako deploy`

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

Builds and deploys the selected app. `--env` defaults to `production`. `development` is reserved for `tako dev`.

Interactive production deploys require confirmation only when the environment is implicit. Passing `--env production`, `--yes`, or `-y` skips the confirmation.

Deploy validates config, builds locally, uploads artifacts to the selected servers, prepares releases, runs the optional leader-only release command, and performs rolling updates.

## `tako logs`

```bash
tako logs
tako logs --env production --days 7
tako logs --env production --tail
tako logs --env production --json
```

Fetches app logs from every server mapped to an environment.

Options:

- `--env <ENV>`: environment to query, default `production`
- `--days <N>`: history window, default `3`
- `--tail`: stream continuously; conflicts with `--days`
- `--json`: emit compact JSONL for automation

History mode sorts lines across servers and pages output when stdout is interactive. Tail mode streams until interrupted.

## `tako releases`

```bash
tako releases ls --env production
tako releases rollback abc1234 --env production --yes
```

Subcommands:

- `ls`, `list`: show release/build history for the current app and environment
- `rollback <release-id>`: roll back using the normal rolling-update path

Rollback to production prompts unless `--yes` or `-y` is provided.

## `tako scale`

```bash
tako scale 2 --env production
tako scale 0 --env production
tako scale 3 --env production --server la
tako scale 0 --server la --app dashboard/production
```

Changes desired instance count per targeted server. Desired count persists across deploys, rollbacks, and server restarts.

In a project directory, Tako resolves the app from the selected config. Outside a project, pass `--app`; use either `<app> --env <env>` or the full deployment id `<app>/<env>`.

Scaling to `0` enables scale-to-zero.

## `tako delete`

```bash
tako delete
tako delete --env production --server la
tako delete --env production --server la --yes
```

Deletes one deployed app target: one app, one environment, one server.

Interactive mode can discover and prompt for the missing target. Non-interactive mode requires `--yes`, `--env`, and `--server`. Outside a project, the flags must still identify exactly one target.

Aliases:

- `rm`
- `remove`
- `undeploy`
- `destroy`

## `tako secrets`

```bash
tako secrets set DATABASE_URL --env production
tako secrets set API_KEY --env production --sync
tako secrets rm API_KEY --env production
tako secrets rm API_KEY --sync
tako secrets ls
tako secrets sync --env production
```

Subcommands:

- `set`, `add`: create or overwrite a secret
- `rm`, `remove`, `delete`, `del`: remove a secret
- `ls`, `list`, `show`: list secret names
- `sync`: push local encrypted secrets to deployed servers

Secret values are prompted with masked input in interactive mode or read from stdin in non-interactive mode. After changes, Tako refreshes generated files best-effort.

### Secret Keys

```bash
tako secrets key export --env production
tako secrets key import --env production
tako secrets key import --passphrase --env production
```

`export` prints a self-contained key bundle. `import` accepts either an exported key bundle or, with `--passphrase`, a passphrase-derived environment key.

## `tako servers`

```bash
tako servers add host.example.com --name la
tako servers add ubuntu@host.example.com --install
tako servers rm la
tako servers ls
tako servers status
tako servers restart la
tako servers restart la --force
tako servers upgrade
tako servers upgrade la
tako servers setup-wildcard
tako servers implode la --yes
```

Subcommands:

| Command                  | Meaning                                                       |
| ------------------------ | ------------------------------------------------------------- | ------------------------------ |
| `add [host               | admin-user@host]`                                             | Add a server to global config. |
| `rm [name]`              | Remove a server from global config.                           |
| `ls`                     | List configured servers.                                      |
| `status`                 | Show global deployment status across servers.                 |
| `restart <name>`         | Gracefully reload `tako-server`.                              |
| `restart <name> --force` | Full service restart.                                         |
| `upgrade [name]`         | Upgrade one or all servers with graceful reload and rollback. |
| `implode [name]`         | Remove remote Tako service, data, and local server entry.     |
| `setup-wildcard`         | Configure DNS-01 wildcard certificate support.                |

Aliases:

- `servers rm`: `remove`, `delete`
- `servers ls`: `list`
- `servers status`: `info`
- `servers implode`: `uninstall`

`servers add` tests SSH as the `tako` user and stores detected target metadata (`arch`, `libc`). With `--install`, or `admin-user@host`, it can install or repair `tako-server` before adding the server.

`servers setup-wildcard` currently applies DNS configuration to all configured servers. The command accepts `--env`, but it does not filter the target server list.

## `tako gen`

```bash
tako gen
```

Generates typed project runtime support:

- JS/TS: `<app_root>/tako.gen.ts`, plus channel/workflow stubs under `<app_root>/channels/` and `<app_root>/workflows/` when relevant
- Go: `tako_secrets.go`

For JS/TS projects, `app_root` comes from `tako.toml` and defaults to `src`. Legacy `tako.d.ts` files are removed when regenerating.

## `tako upgrade`

```bash
tako upgrade
```

Upgrades the local CLI installation. On macOS it preserves the app-bundle layout and symlinked CLI path.

## `tako version`

```bash
tako version
tako --version
```

Prints version information. Rolling builds report `<base>-<sha7>`.

## `tako help`

```bash
tako help
tako <command> --help
```

Shows command usage.

## `tako implode`

```bash
tako implode
tako implode --yes
tako uninstall --yes
```

Removes the local Tako CLI and local data. On macOS and Linux it also removes local development system services/configuration installed by `tako dev`, which may require sudo.
