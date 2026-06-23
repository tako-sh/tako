---
layout: ../../layouts/DocsLayout.astro
title: "Tako CLI Reference - Tako Docs"
heading: CLI Reference
current: cli
description: "Complete CLI reference for Tako commands including init, dev, run, deploy, servers, secrets, storage, status, logs, and global flags."
---

# CLI Reference

```bash
tako [--version] [-v|--verbose] [--ci] [--json] [--dry-run] [-c|--config <CONFIG>] [--ssh-passphrase <PASSPHRASE>] <command>
```

Progress, prompts, diagnostics, and logs go to stderr. Command results and machine-readable data go to stdout.

## Global Options

| Option                          | Meaning                                                                                                                         |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `--version`                     | Print the CLI version and exit.                                                                                                 |
| `-v`, `--verbose`               | Append-only transcript with timestamps, log levels, and debug diagnostics.                                                      |
| `--ci`                          | Deterministic non-interactive output: no colors, spinners, raw mode, or prompts.                                                |
| `--json`                        | Emit structured JSON on stdout. Progress, diagnostics, and errors stay on stderr.                                               |
| `--dry-run`                     | Show side effects without performing them. Supported by deploy, servers add/remove, delete, and side-effecting backup commands. |
| `-c`, `--config <CONFIG>`       | Use an explicit config file. `.toml` is appended when omitted.                                                                  |
| `--ssh-passphrase <PASSPHRASE>` | Use a passphrase for encrypted local SSH keys.                                                                                  |

App-scoped commands that honor `-c`: `init`, `dev`, `run`, `logs`, `deploy`, `releases`, `backups`, `delete`, `secrets`, `storages`, `generate`, and project-context `scale`.

For finite commands, `--json` prints one final object. Commands without a specialized schema use `{"ok":true,"command":"<command>"}`. Failures print `{"ok":false,"error":{"message":"..."}}` on stdout and the human-readable error on stderr. `tako logs --tail --json` is the streaming exception: it emits one structured log event per stdout line until interrupted. `tako run` is also an exception: child stdout stays untouched and no JSON result object is appended.

## Project Setup

```bash
tako init
```

Creates `tako.toml`, updates `.gitignore`, detects runtime and preset, asks for app name and production route, and installs the SDK package for JS and Go projects. If the production route is a wildcard route, init can collect and encrypt the Cloudflare credential required for Let's Encrypt DNS-01.

```bash
tako generate
tako gen
tako g
```

Refreshes generated project files:

- JS/TS: `tako.d.ts` with environment names, secret names, storage bindings, channel metadata, workflow metadata, and env var names.
- Go: `tako_secrets.go` with typed secret accessors.
  For JS/TS, generation keeps an existing `tako.d.ts` in `app/`, `src/`, or the project root. Legacy `tako.gen.ts` files are removed. Empty existing `channels/` or `workflows/` directories get demo definitions.

## `tako dev`

```bash
tako dev
tako dev --variant preview
tako dev --var preview
tako dev --tunnel
```

Starts or attaches to a local dev session behind trusted HTTPS and real hostnames. It starts the dev daemon, prepares DNS/proxy/CA setup, generates files, injects secrets and storage through fd 3, waits for fd-4 readiness, and registers routes.

Interactive controls:

| Key      | Action                                                             |
| -------- | ------------------------------------------------------------------ |
| `l`      | Toggle LAN mode for managed local routes.                          |
| `t`      | Toggle a temporary public tunnel URL.                              |
| `r`      | Restart the app.                                                   |
| `b`      | Leave the app running in the background and exit the attached CLI. |
| `Ctrl+c` | Stop the app and exit.                                             |

The interactive status panel always shows local routes plus LAN and tunnel state. LAN and tunnel rows include their own enable/disable hints on the same rows; tunnel also shows starting and reconnecting states while it connects. Tunnel hostnames use `{app}-{id}.tako.website`; the id is derived from the app name and Tako Identity, so the same app gets the same URL when the same identity is available. If the tunnel connection drops, Tako keeps the URL reserved and reconnects automatically. One Tako Identity can have up to five active tunnel URLs connected at the same time; starting a sixth closes the oldest active tunnel for that identity. When a tunnel turns off, `tako dev` prints a log line with the close reason.

Inactive tunnel URLs show a Tako error page in browsers and machine-readable errors for API clients.

Subcommands:

```bash
tako dev stop [name]
tako dev stop --all
tako dev list
tako dev ls
```

`stop` without a name stops the app for the selected config file. `--all` stops all registered dev apps. `list` shows currently registered dev apps and any active tunnel URLs.

## `tako doctor`

```bash
tako doctor
```

Prints local diagnostics for the dev daemon, local DNS, TLS files, and platform-specific proxy setup.

## `tako run`

```bash
tako run --env development -- bun scripts/foo.ts
tako run --env staging --secrets-as-env -- bun scripts/foo.ts
```

Runs a one-off command locally from the app directory with Tako project context. `--env` defaults to `development`.

The child process receives `[vars]` plus `[vars.<env>]`, `ENV`, `TAKO_BUILD=local`, `TAKO_DATA_DIR`, runtime defaults, and `TAKO_APP_ROOT` for JS apps. Tako decrypts local app secrets for the selected environment and passes the normal bootstrap envelope through `TAKO_BOOTSTRAP_DATA`, so SDK-aware scripts use `tako.secrets` and `tako.storages`.

Secrets are not process env vars by default. Use `--secrets-as-env` only for tools that cannot use the SDK. `tako run` is local-only in v0; it does not run commands on deployed servers.

## Development

```bash
tako dev [--variant <name>] [--tunnel]
tako dev stop [name] [--all]
tako dev list
tako dev ls
```

`tako dev` starts or attaches to the local dev daemon and registers the selected app config. `--variant foo` runs a variant hostname such as `myapp-foo.test`. `--tunnel` starts with a temporary public tunnel URL.

Interactive shortcuts: `r` restarts, `l` toggles LAN `.local` aliases, `t` toggles tunnel mode, `b` backgrounds the app, and `Ctrl-C` unregisters it.

The interactive status panel always shows local routes plus LAN and tunnel state. LAN and tunnel rows include enable/disable hints, and tunnel shows starting or reconnecting state while it connects. Tunnel hostnames use `{app}-{id}.tako.website`; the id is derived from the app name and Tako Identity, so the same app gets the same URL when the same identity is available. One Tako Identity can have up to five active tunnel URLs connected at the same time; starting a sixth closes the oldest active tunnel for that identity. Inactive tunnel URLs show a Tako error page in browsers and machine-readable errors for API clients.

## Deploy And Runtime Operations

```bash
tako deploy [--env <env>] [-y|--yes]
```

Builds locally, uploads artifacts over signed HTTP management, prepares every mapped server, runs the optional release command on the leader, rolls instances, finalizes the release, and runs post-deploy backups when enabled. `--env` defaults to `production`. `development` is reserved for `tako dev`.

```bash
tako logs [--env <env>] [--tail] [--days N]
```

Shows app logs from all servers mapped to the environment. History defaults to three days. `--tail` streams. Global `--json` emits a final object with a `logs` array in history mode; with `--tail`, it emits JSONL records.

```bash
tako scale <instances> [--env <env>] [--server <server>] [--app <app>]
```

Changes desired instance count on targeted servers. From a project, app identity is resolved from the config. Outside a project, pass `--app`, either as a base app with `--env` or as a full `{app}/{env}` deployment id.

```bash
tako delete [--env <env>] [--server <server>] [-y|--yes]
tako rm ...
tako remove ...
tako undeploy ...
tako destroy ...
```

Deletes one deployment target, drains and stops the app, removes runtime state, and removes the server-side app data tree for that app/environment on the selected server.

## Releases

```bash
tako releases list [--env <env>]
tako releases ls [--env <env>]
tako releases rollback <release-id> [--env <env>] [-y|--yes]
```

Lists release/build history or rolls an environment back to a previous release. Rollback uses the standard rolling-update flow and reports partial server failures individually.

## Servers

```bash
tako servers add [host|admin@host] [--name <name>] [--description <text>] [--port <ssh-port>] [--http-port <port>] [--https-port <port>] [--install] [--admin-user <user>]
tako servers list
tako servers ls
tako status
tako servers reload <name> [--force]
tako servers upgrade [name]
tako servers remove [name]
tako servers rm [name]
tako servers uninstall [name] [-y|--yes]
```

`servers add` writes global `config.toml`, verifies SSH recovery access, enrolls signed management access, records target metadata, and can install or repair `tako-server` first. Passing `admin@host` is shorthand for install/repair with that admin user.

`status` prints a deployment snapshot grouped by server, with compact server summary, routes, and app rows.

`servers reload` performs a zero-downtime service reload by default. `--force` performs a full restart. `servers upgrade` installs a new server binary and reloads with rollback to the previous binary if readiness fails. `servers uninstall` removes the remote service, binaries, data, sockets, and local server entry.

## Secrets

```bash
tako secrets set <NAME> [--env <env>] [--expires-on <when>] [--sync]
tako secrets add <NAME> ...
tako secrets rm <NAME> [--env <env>] [--sync]
tako secrets list
tako secrets sync [--env <env>]
tako secrets key export [--env <env>]
tako secrets key import [--env <env>] [--passphrase]
```

Secrets are app-facing values delivered to SDKs. Values are encrypted in `.tako/secrets.json`; keys are stored outside the project. Interactive prompts accept pasted multiline values and ask before trimming surrounding whitespace; non-interactive commands read the full stdin stream. `--sync` sends updates to deployed servers and rolls fresh processes so they receive the new bootstrap envelope.

Expiry accepts `YYYY-MM-DD`, `in N days`, or `never`. Deploy fails on expired selected-environment secrets and warns when they expire within 30 days.

## Provider Credentials

```bash
tako credentials set [name] --env <env> [--expires-on <when>]
tako creds set ...
tako credentials rm <name> --env <env>
tako credentials list
```

Provider credentials are Tako-owned runtime credentials, not app secrets. Supported names are `ssl.cloudflare` and `postgres_url`. They are encrypted in `.tako/secrets.json`, never exposed to app code, and not synced by `tako secrets sync`.

`ssl.cloudflare` powers Let's Encrypt DNS-01 and Cloudflare Origin CA. `postgres_url` enables shared channel/workflow storage for multi-server deployments.

## Storage

```bash
tako storages add <binding> [--env <env>] [--resource <resource>] [--provider s3|local] [--bucket <bucket>] [--endpoint <https-url>] [--region <region>] [--access-key-id <value>] [--secret-access-key <value>] [--expires-on <when>] [--force-path-style] [--public-base-url <https-url>]
tako storages credentials <resource> [--env <env>] [--access-key-id <value>] [--secret-access-key <value>] [--expires-on <when>]
```

`storages add` writes the app binding to `tako.toml`. For S3 resources it also writes resource metadata and encrypted credentials. `storages credentials` rotates credentials for an existing top-level S3 resource without adding an app binding, which is useful for backup-only resources.

## Backups

```bash
tako backups now [--env <env>] [--server <server>]
tako backups list [--env <env>] [--server <server>]
tako backups ls [--env <env>] [--server <server>]
tako backups status [--env <env>]
tako backups download <backup-id> [--env <env>] [--server <server>] [--output <path>]
tako backups restore <backup-id> [--env <env>] [--server <server>] [-y|--yes]
```

Backup commands default to `production`, require project context, and target the deployment id `{app}/{env}` over signed HTTP management. Multi-server download and restore require `--server`.

## Local CLI Maintenance

```bash
tako upgrade
tako uninstall [-y|--yes]
tako version
```

`upgrade` updates the local CLI install. On macOS, official CLI upgrades support Apple Silicon only. `uninstall` removes local Tako binaries, local data, and platform-specific dev services/config after confirmation.
