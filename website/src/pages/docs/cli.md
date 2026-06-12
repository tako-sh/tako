---
layout: ../../layouts/DocsLayout.astro
title: "Tako CLI Reference - Tako Docs"
heading: CLI Reference
current: cli
description: "Complete CLI reference for Tako commands including init, dev, deploy, servers, secrets, storage, status, logs, and global flags."
---

# CLI Reference

```bash
tako [--version] [-v|--verbose] [--ci] [--dry-run] [-c|--config <CONFIG>] [--ssh-passphrase <PASSPHRASE>] <command>
```

Progress, prompts, diagnostics, and logs go to stderr. Command results and machine-readable data go to stdout.

## Global Options

| Option                          | Meaning                                                                                                                         |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `--version`                     | Print the CLI version and exit.                                                                                                 |
| `-v`, `--verbose`               | Append-only transcript with timestamps, log levels, and debug diagnostics.                                                      |
| `--ci`                          | Deterministic non-interactive output: no colors, spinners, raw mode, or prompts.                                                |
| `--dry-run`                     | Show side effects without performing them. Supported by deploy, servers add/remove, delete, and side-effecting backup commands. |
| `-c`, `--config <CONFIG>`       | Use a config file other than `./tako.toml`; `.toml` is appended when omitted.                                                   |
| `--ssh-passphrase <PASSPHRASE>` | Passphrase for encrypted local SSH keys used by SSH and signed management requests.                                             |

App-scoped commands treat the selected config file's parent directory as the app directory. This includes `init`, `dev`, `logs`, `deploy`, `releases`, `backups`, `delete`, `secrets`, `storages`, `generate`, and project-context `scale`.

## `tako init`

```bash
tako init
tako init -c apps/web/tako.toml
```

Creates `tako.toml`, updates `.gitignore`, detects runtime and preset, asks for app name and production route, and installs the SDK package for JS, Go, and Rust projects. If the production route is a wildcard route, init can collect and encrypt the Cloudflare credential needed for Let's Encrypt DNS-01.

Existing config files are protected: interactive runs ask before overwrite; non-interactive runs leave the file untouched.

## `tako generate`

Aliases: `tako gen`, `tako g`.

```bash
tako generate
```

Refreshes generated project files:

- JS/TS: `tako.d.ts` with environment names, secret names, storage bindings, channel metadata, workflow metadata, and env var names.
- Go: `tako_secrets.go` with typed secret accessors.
- Rust: no generated files today.

For JS/TS, generation keeps an existing `tako.d.ts` in `app/`, `src/`, or the project root. Legacy `tako.gen.ts` files are removed. Empty existing `channels/` or `workflows/` directories get demo definitions.

## `tako dev`

```bash
tako dev
tako dev --variant preview
tako dev --var preview
```

Starts or attaches to a local dev session behind trusted HTTPS and real hostnames. It starts the dev daemon, prepares DNS/proxy/CA setup, generates files, injects secrets and storage through fd 3, waits for fd-4 readiness, and registers routes.

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
tako dev ls
```

`stop` without a name stops the app for the selected config file. `--all` stops all registered dev apps. `list` shows currently registered dev apps.

## `tako doctor`

```bash
tako doctor
```

Prints a local diagnostic report for the dev daemon, macOS launchd proxy, Linux redirect/DNS setup, local CA, loopback address, and repair hints. If the dev daemon is simply not running, doctor reports that and exits successfully.

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

Deploy validates config, secrets, storage credentials, provider credentials, runtime state storage, backup config, route setup, and server target metadata before build work starts. It builds locally, packages an artifact, uploads it over signed HTTP management, prepares each server release, optionally runs the `release` command once on the leader server, performs rolling updates, finalizes the release, and creates a post-deploy backup when enabled.

`development` is reserved for `tako dev` and cannot be deployed. Interactive production deploys ask for confirmation only when the environment is implicit.

## `tako logs`

```bash
tako logs --env production
tako logs --env production --tail
tako logs --env production --days 7
tako logs --env production --json
```

| Option        | Meaning                                                        |
| ------------- | -------------------------------------------------------------- |
| `--env <ENV>` | Environment to read logs for. Defaults to `production`.        |
| `--tail`      | Stream logs until interrupted. Conflicts with `--days`.        |
| `--days <N>`  | Fetch historical logs from the last `N` days. Defaults to `3`. |
| `--json`      | Emit JSONL for agents and automation.                          |

Logs are read from all mapped servers over signed HTTP management. Human output formats timestamps, level, source, and message, prefixes server names when needed, deduplicates consecutive repeats, and opens a pager in interactive history mode.

Proxy diagnostics include request ID, selected app and instance, route, handler/cache result, status, total latency, cold-start wait time, upstream response-header latency, and response compression fields when available. Tako forwards the request ID to upstream apps as `X-Request-ID` so app logs can correlate with proxy events.

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
| `--port <PORT>`        | SSH port. Defaults to `22`.                                        |
| `--http-port <PORT>`   | Public HTTP port used by installer flows.                          |
| `--https-port <PORT>`  | Public HTTPS port used by installer flows.                         |
| `--install`            | Install or repair `tako-server` before adding.                     |
| `--admin-user <USER>`  | SSH user for `--install`.                                          |

`admin-user@host` is shorthand for selecting an admin SSH user and enabling install/repair when needed. Add verifies Tailscale reachability, SSH recovery access, signed HTTP management, server identity, and target metadata before writing `config.toml`.

### `tako servers remove`

Aliases: `rm`, `delete`.

```bash
tako servers remove prod-a
```

Removes a server from global config. It does not uninstall the remote service.

### `tako servers list`

Alias: `ls`.

```bash
tako servers list
```

Lists configured servers, host, SSH port, public HTTP/HTTPS ports, and description.

### `tako servers status`

Alias: `info`.

```bash
tako servers status
```

Shows one snapshot of configured servers and deployed app/build state. It does not require `tako.toml` and can run from any directory.

### `tako servers reload`

```bash
tako servers reload prod-a
tako servers reload prod-a --force
```

Reloads `tako-server` without downtime by default. `--force` performs a full service restart and may briefly interrupt apps.

### `tako servers upgrade`

```bash
tako servers upgrade
tako servers upgrade prod-a
```

Upgrades `tako-server` on one or all configured servers via service-manager reload. It installs the new binary, verifies checksums, enters upgrade mode, reloads the service, waits for readiness, and exits upgrade mode. Custom `TAKO_DOWNLOAD_BASE_URL` sources skip signature verification for the custom checksum manifest but still verify the archive SHA-256 after download.

### `tako servers uninstall`

```bash
tako servers uninstall prod-a
tako servers uninstall prod-a --yes
```

Removes `tako-server`, services, helpers, binaries, data, sockets, and local server inventory entry for the selected server.

## `tako credentials`

Alias group: `tako creds`.

```bash
tako credentials
tako credentials set
tako credentials set ssl.cloudflare --env production --expires-on "in 90 days"
tako credentials set postgres_url --env production
tako credentials rm ssl.cloudflare --env production
tako credentials list
```

| Command      | Meaning                                                                                |
| ------------ | -------------------------------------------------------------------------------------- |
| none         | Show supported provider credentials and which environments have values set.            |
| `set [NAME]` | Store an encrypted provider credential. Omit `NAME` to choose one interactively.       |
| `rm <NAME>`  | Remove a provider credential from one environment. Aliases: `remove`, `delete`, `del`. |
| `list`       | List credential names and environments. Aliases: `ls`, `show`.                         |

Supported provider credentials today: `ssl.cloudflare` and `postgres_url`. Credential names are lowercased before validation, so `POSTGRES_URL` is stored as `postgres_url`. Interactive terminals show a selector when `NAME` is omitted.

Provider credentials are for deployed environments. `development` is omitted from the selector and rejected by `--env`; use `production` or another deployment environment. Provider credentials are encrypted in `.tako/secrets.json`, not exposed to app code, not included in generated secret types, and not pushed by `tako secrets sync`. Deploy sends them only through the deployment binding that needs them. `postgres_url` selects shared Postgres storage for channels and workflows.

## `tako secrets`

### `tako secrets set`

Alias: `add`.

```bash
tako secrets set DATABASE_URL --env production --expires-on "in 90 days"
tako secrets set DATABASE_URL --env production --sync
```

Stores or updates an encrypted app secret. Interactive runs prompt for environment, value, optional expiry, and overwrite confirmation when needed. Non-interactive runs read one value line from stdin and require `--env`.

### `tako secrets rm`

Aliases: `remove`, `delete`, `del`.

```bash
tako secrets rm DATABASE_URL --env production
tako secrets rm DATABASE_URL --sync
```

Removes a secret from one environment, or from all environments when `--env` is omitted. `--sync` pushes the resulting secret set to mapped servers.

### `tako secrets list`

Aliases: `ls`, `show`.

```bash
tako secrets list
```

Shows a presence table across environments. Values are never printed.

### `tako secrets sync`

```bash
tako secrets sync
tako secrets sync --env production
```

Decrypts local secrets and sends `update_secrets` to mapped servers over signed HTTP management. Remote updates restart workflow workers and roll HTTP instances.

### `tako secrets key export`

```bash
tako secrets key export --env production
```

Exports a self-contained key bundle and copies it to the clipboard.

### `tako secrets key import`

```bash
tako secrets key import --env production
tako secrets key import --passphrase --env production
```

Imports an exported key or passphrase-derived key. Non-interactive mode reads the exported key or passphrase from stdin.

## `tako storages`

### `tako storages add`

```bash
tako storages add uploads \
  --env production \
  --resource prod_uploads \
  --provider s3 \
  --bucket app-uploads \
  --endpoint https://<account>.r2.cloudflarestorage.com \
  --region auto \
  --public-base-url https://cdn.example.com/uploads

tako storages add uploads --env development --provider local
```

Attaches an app storage binding. `--env` defaults to `production`, `--provider` defaults to `s3`, and `--resource` defaults to the binding name for S3. S3 options include bucket, endpoint, region, access key id, secret access key, expiry, path-style signing, and public base URL.

For `local`, omit S3-only options. It writes a binding to the built-in `local` resource and no credentials.

### `tako storages credentials`

```bash
tako storages credentials prod_uploads --env production
```

Sets or rotates encrypted credentials for an existing top-level S3 resource without adding an app binding. This is useful for backup-only storage resources.

## `tako backups`

```bash
tako backups now --env production
tako backups list --env production
tako backups status --env production
tako backups download b123 --env production --server prod-a --output ./backup.tar.zst.enc
tako backups restore b123 --env production --server prod-a --yes
```

| Command                | Meaning                                                                                          |
| ---------------------- | ------------------------------------------------------------------------------------------------ |
| `now`                  | Create backups immediately on selected server(s).                                                |
| `list`                 | List remote backup index entries. Alias: `ls`.                                                   |
| `status`               | Show enabled state, retention, latest backup, and next due time.                                 |
| `download <backup-id>` | Download one encrypted archive. `--server` is required for multi-server environments.            |
| `restore <backup-id>`  | Stop the selected app, replace its data tree, reconcile workflows, and restart to desired count. |

Backup commands default to `production`, require project context, and use signed HTTP management.

## `tako releases`

```bash
tako releases list --env production
tako releases rollback abc1234 --env production --yes
```

`list` (alias `ls`) merges release history from mapped servers. `rollback` points the app/environment back to a previous release/build id on each mapped server. Production rollback asks for confirmation unless `--yes` is passed.

## `tako scale`

```bash
tako scale 0 --env production
tako scale 2 --env production
tako scale 0 --server prod-a --app my-app/production
```

Changes the desired instance count per targeted server. The count persists across server restarts, deploys, and rollbacks. Requests above the app's effective server maximum fail instead of being silently capped. In project context, `--env` defaults to `production`; outside a project, provide `--server` and `--app`.

## `tako delete`

Aliases: `rm`, `remove`, `undeploy`, `destroy`.

```bash
tako delete --env production --server prod-a --yes
```

Deletes exactly one deployment target. It drains and stops the app on that server, removes runtime registration, routes, state, and app data. Use `--server` when the environment maps to multiple servers.

## `tako upgrade`

```bash
tako upgrade
```

Upgrades the local CLI installation. Homebrew installs use `brew upgrade tako`; other installs download the hosted CLI archive, verify its checksum, and preserve the installer layout. On macOS, the signed `Tako.app` bundle is installed atomically and `tako` remains a symlink to the CLI inside the app bundle.

## `tako uninstall`

```bash
tako uninstall
tako uninstall --yes
```

Removes the local Tako CLI, local data, dev daemon state, local CA trust, and platform-specific dev proxy/DNS/redirect setup. System-level cleanup may require sudo.
