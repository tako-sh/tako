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

Progress, prompts, status, and logs go to stderr. Command results and machine-readable data go to stdout.

## Global Options

| Option                          | Meaning                                                                                              |
| ------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `--version`                     | Print CLI version.                                                                                   |
| `-v`, `--verbose`               | Enable debug diagnostics and detailed progress logs.                                                 |
| `--ci`                          | Disable interactive prompts and pretty UI.                                                           |
| `--dry-run`                     | Show side effects without performing them. Supported by deploy, server add/remove, and delete flows. |
| `-c`, `--config <CONFIG>`       | Use a specific app config file. If it has no `.toml` suffix, Tako appends it.                        |
| `--ssh-passphrase <PASSPHRASE>` | Passphrase for encrypted SSH keys used by server operations.                                         |

App-scoped commands treat the selected config file's parent directory as the app directory. This includes `init`, `dev`, `logs`, `deploy`, `releases`, `delete`, `secrets`, `storages`, `dns`, `generate`, and project-context `scale`.

## `tako init`

Create a new `tako.toml`:

```bash
tako init
tako init -c apps/web/tako.toml
```

Interactive init detects or prompts for runtime, preset, route, server membership, and production route. For JS projects it also chooses `app_root` for channel and workflow discovery.

For JavaScript runtimes, init installs `tako.sh` with the selected package manager. For Go, it runs `go get tako.sh`.

If the production route is a wildcard route, init offers to configure Cloudflare DNS for wildcard HTTPS and stores the token encrypted in `.tako/secrets.json`.

## `tako generate`

Aliases: `tako gen`, `tako g`.

```bash
tako generate
```

Generates project files from config:

- JS/TS: `tako.d.ts` with typed runtime metadata, environments, secrets, storages, channels, and workflows.
- Go: `tako_secrets.go` with typed secret accessors.

For JS/TS projects, generation keeps an existing `tako.d.ts` in `app/`, `src/`, or the project root. Legacy `tako.gen.ts` files are removed on regeneration.

If JS channel or workflow directories exist and are empty, generation scaffolds demo definitions.

## `tako dev`

```bash
tako dev
tako dev --variant staging
```

`tako dev` runs the app behind local HTTPS and real development routes. It starts or reuses the local dev daemon, prepares local DNS and proxying, generates project files, injects secrets and storage bindings through fd 3, and waits for fd-4 readiness before routes become active.

`--variant` selects a named development variant. Variants get isolated local runtime state while sharing project config.

Interactive controls:

| Key      | Action                                                             |
| -------- | ------------------------------------------------------------------ |
| `l`      | Toggle LAN mode for managed local routes.                          |
| `r`      | Restart the app.                                                   |
| `b`      | Leave the app running in the background and exit the attached CLI. |
| `Ctrl+c` | Stop the app and exit.                                             |

Subcommands:

```bash
tako dev stop [name]
tako dev stop --all
tako dev list
```

`dev list` also has alias `dev ls`.

## `tako doctor`

```bash
tako doctor
```

Reports local dev setup, daemon state, macOS or Linux proxy/DNS status, loopback configuration, and useful repair hints. It exits successfully when the dev daemon is simply not running.

## `tako deploy`

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

| Option        | Meaning                                          |
| ------------- | ------------------------------------------------ |
| `--env <ENV>` | Environment to deploy. Defaults to `production`. |
| `-y`, `--yes` | Skip confirmation prompts.                       |

`development` is reserved for `tako dev` and cannot be deployed.

Interactive production deploys ask for confirmation only when the environment is implicit. Passing `--env production` or `--yes` makes the target explicit and skips that prompt.

Deploy validates secrets, storage credentials, wildcard DNS credentials, routes, target servers, and server target metadata before build work starts. It builds locally, packages an artifact, uploads to each server, prepares the release, optionally runs the release command, and performs a rolling update.

Wildcard routes require `tako dns configure --env <env>`. Storage bindings configured with `tako storages add` are synced during deploy; there is no separate storage sync command.

## `tako logs`

```bash
tako logs --env production
tako logs --env production --tail
tako logs --env production --days 7
tako logs --env production --json
```

| Option        | Meaning                                                  |
| ------------- | -------------------------------------------------------- |
| `--env <ENV>` | Environment to read logs for. Defaults to `production`.  |
| `--tail`      | Stream live logs.                                        |
| `--days <N>`  | Fetch historical logs from the last N days. Default `3`. |
| `--json`      | Emit JSON lines.                                         |

Historical logs are sorted by timestamp across target servers. Interactive history output opens in a pager when stdout is a terminal.

## `tako servers`

Server inventory is global user config, not app config.

### `tako servers add`

```bash
tako servers add prod-a.tailnet.ts.net
tako servers add ubuntu@prod-a.tailnet.ts.net
tako servers add prod-a.tailnet.ts.net --install --admin-user ubuntu
```

| Option                 | Meaning                                                            |
| ---------------------- | ------------------------------------------------------------------ |
| `--name <NAME>`        | Server name. Defaults to the host's first DNS label when possible. |
| `--description <TEXT>` | Optional description shown in server lists.                        |
| `--port <PORT>`        | SSH port. Default `22`.                                            |
| `--http-port <PORT>`   | Public HTTP port used by installs.                                 |
| `--https-port <PORT>`  | Public HTTPS port used by installs.                                |
| `--install`            | Install or repair `tako-server` before adding.                     |
| `--admin-user <USER>`  | SSH user for `--install`.                                          |

Passing `admin-user@host` is shorthand for using that admin user and enabling install or repair when needed.

`servers add` no longer runs an app DNS/configure wizard. It adds or starts the server with default listener settings; app routing, source-IP, storage, and DNS bindings are applied by `tako deploy`.

### `tako servers remove`

Aliases: `tako servers rm`, `tako servers delete`.

```bash
tako servers remove prod-a
```

Removes the server entry from global config. It does not uninstall the remote service.

### `tako servers list`

Alias: `tako servers ls`.

```bash
tako servers list
```

Lists configured servers, host, SSH port, public HTTP/HTTPS ports, and description.

### `tako servers status`

Alias: `tako servers info`.

```bash
tako servers status
```

Shows deployment status across configured servers. It does not require `tako.toml` and can run from any directory.

### `tako servers reload`

```bash
tako servers reload prod-a
tako servers reload prod-a --force
```

Reloads `tako-server` without downtime by default. `--force` performs a full restart with brief downtime.

### `tako servers upgrade`

```bash
tako servers upgrade
tako servers upgrade prod-a
```

Upgrades one server or all servers through graceful reload with rollback to the previous binary on failure.

### `tako servers uninstall`

```bash
tako servers uninstall prod-a
tako servers uninstall prod-a --yes
```

Removes `tako-server`, service files, app data, runtime data, authorized keys, and the local server inventory entry.

## `tako dns configure`

```bash
tako dns configure --env production
tako dns configure --env production --cloudflare-api-token "$TOKEN" --expires-at "in 90 days"
```

| Option                           | Meaning                                                                |
| -------------------------------- | ---------------------------------------------------------------------- |
| `--env <ENV>`                    | Environment to configure. Default `production`.                        |
| `--cloudflare-api-token <TOKEN>` | Token value. Prompted when omitted.                                    |
| `--expires-at <WHEN>`            | Optional expiry: `YYYY-MM-DD`, `in N days`, UTC timestamp, or `never`. |

Stores encrypted Cloudflare DNS credentials in `.tako/secrets.json` under the selected environment's `dns` object. It does not edit `tako.toml`.

Deploy sends DNS credentials to servers only for wildcard routes in that app environment.

## `tako secrets`

### `tako secrets set`

Alias: `tako secrets add`.

```bash
tako secrets set DATABASE_URL --env production
printf '%s\n' "$DATABASE_URL" | tako secrets set DATABASE_URL --env production --expires-at "in 90 days"
```

| Option                | Meaning                                                                          |
| --------------------- | -------------------------------------------------------------------------------- |
| `--env <ENV>`         | Target environment. Interactive terminals can choose or create one when omitted. |
| `--expires-at <WHEN>` | Optional expiry: `YYYY-MM-DD`, `in N days`, UTC timestamp, or `never`.           |
| `--sync`              | Sync to servers after saving.                                                    |

Secret values are encrypted in `.tako/secrets.json`. Expired selected secrets fail deploy before build work starts; secrets expiring within 30 days produce a warning.

### `tako secrets rm`

Aliases: `remove`, `delete`, `del`.

```bash
tako secrets rm DATABASE_URL --env production
tako secrets rm DATABASE_URL --sync
```

Removes a secret from one environment or, when `--env` is omitted, all environments after confirmation.

### `tako secrets list`

Aliases: `ls`, `show`.

```bash
tako secrets list
```

Lists secret names and the environments where each is set. Values are never printed.

### `tako secrets sync`

```bash
tako secrets sync
tako secrets sync --env production
```

Syncs decrypted app secrets to deployed servers over the signed management path. The server stores them encrypted, drains/restarts workflow workers, and rolls HTTP instances so fresh processes receive the new values.

### `tako secrets key export`

```bash
tako secrets key export --env production
```

Copies a self-contained environment key bundle to the clipboard after local export authentication.

### `tako secrets key import`

```bash
tako secrets key import --env production
tako secrets key import --passphrase --env production
```

Imports an exported key or passphrase-derived key for an environment.

## `tako storages`

```bash
tako storages add uploads \
  --env production \
  --provider s3 \
  --bucket my-app-prod \
  --endpoint https://example.r2.cloudflarestorage.com \
  --region auto
```

| Option                        | Meaning                                                    |
| ----------------------------- | ---------------------------------------------------------- | ------------------------------- |
| `name`                        | App-facing binding name exposed as `tako.storages.<name>`. |
| `--env <ENV>`                 | Environment to attach storage to. Default `production`.    |
| `--resource <NAME>`           | Backing S3 resource name. Defaults to the binding name.    |
| `--provider <local            | s3>`                                                       | Storage provider. Default `s3`. |
| `--bucket <BUCKET>`           | Required for S3.                                           |
| `--endpoint <URL>`            | Required HTTPS S3-compatible endpoint.                     |
| `--region <REGION>`           | Region. Use `auto` for R2.                                 |
| `--access-key-id <VALUE>`     | Access key id. Prompted when omitted for S3.               |
| `--secret-access-key <VALUE>` | Secret access key. Prompted when omitted for S3.           |
| `--expires-at <WHEN>`         | Optional S3 credential expiry.                             |
| `--force-path-style`          | Use path-style bucket URLs.                                |
| `--public-base-url <URL>`     | HTTPS public base URL for public object URLs.              |

The command writes binding metadata to `tako.toml`. S3 resources also write non-secret provider metadata and encrypted credentials. Local storage writes the binding to the built-in `local` resource, no `[storages.local]` table, and `--resource` may be omitted or set to `local`. Deploy syncs storage bindings; there is no separate storage sync command.

## `tako releases`

### `tako releases list`

Alias: `tako releases ls`.

```bash
tako releases list --env production
```

Lists deployed release history for the current app environment across target servers, including current marker, deploy time, commit message, and dirty/clean status when available.

### `tako releases rollback`

```bash
tako releases rollback abc1234 --env production
tako releases rollback abc1234 --env production --yes
```

Rolls the current app environment back to a previous release id. Production rollback asks for confirmation in interactive terminals unless `--yes` is passed.

## `tako scale`

```bash
tako scale 0 --env production
tako scale 2 --env production
tako scale 0 --server prod-a --app my-app/production
```

| Option            | Meaning                                                               |
| ----------------- | --------------------------------------------------------------------- |
| `instances`       | Desired instance count from `0` to `255`.                             |
| `--env <ENV>`     | Project environment target.                                           |
| `--server <NAME>` | Specific configured server.                                           |
| `--app <APP>`     | App name or deployed app id when running outside a project directory. |

Scale settings persist per targeted server across restarts, deploys, and rollbacks. `0` enables scale-to-zero.

## `tako delete`

Aliases: `tako rm`, `tako remove`, `tako undeploy`, `tako destroy`.

```bash
tako delete --env production --server prod-a --yes
```

| Option            | Meaning                           |
| ----------------- | --------------------------------- |
| `--env <ENV>`     | Environment to delete.            |
| `--server <NAME>` | Server deployment to delete from. |
| `-y`, `--yes`     | Skip confirmation.                |

Delete removes one deployed app/environment/server target after draining instances and workers. In non-interactive mode, pass `--yes`, `--env`, and `--server`.

## `tako upgrade`

```bash
tako upgrade
```

Upgrades the local CLI. Homebrew installs upgrade through Homebrew; other installs use the release installer path.

## `tako uninstall`

```bash
tako uninstall
tako uninstall --yes
```

Removes local Tako development components and config after confirmation.

## `tako version`

```bash
tako version
```

Prints the CLI version. `tako --version` is equivalent for simple version output.
