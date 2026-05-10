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

| Flag                            | Description                                                                     |
| ------------------------------- | ------------------------------------------------------------------------------- |
| `--version`                     | Print version and exit.                                                         |
| `-v`, `--verbose`               | Show an append-only transcript with timestamps and debug detail.                |
| `--ci`                          | Deterministic non-interactive output: no color, spinners, or prompts.           |
| `--dry-run`                     | Show planned side effects without performing them where supported.              |
| `-c`, `--config <CONFIG>`       | Select an app config file instead of `./tako.toml`; `.toml` suffix is optional. |
| `--ssh-passphrase <PASSPHRASE>` | Use this passphrase for encrypted local SSH keys.                               |

App-scoped commands use the selected config file's parent directory as the app directory. `--dry-run` is supported by `deploy`, `servers add`, `servers rm`, and `delete`.

## Installation

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

Upgrade the local CLI:

```bash
tako upgrade
```

GitHub-backed update checks and release downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.
On macOS, `tako upgrade` preserves the signed `Tako.app` installation and keeps `tako` linked to the CLI inside the app bundle.

## `tako init`

Create a project config and install the SDK.

```bash
tako init
tako init -c staging
```

`init` detects the runtime, offers preset choices, writes `tako.toml`, updates `.gitignore`, pins the local runtime version when possible, and installs `tako.sh` through the selected package manager.

The generated config includes commented examples for runtime fields, routes, variables, `[build]`, `[[build_stages]]`, assets, dev commands, and idle scaling. If the config file already exists, interactive terminals ask before overwriting. Non-interactive runs leave it untouched.

## `tako dev`

Start or attach to a local development session.

```bash
tako dev
tako dev --variant preview
tako dev --var preview
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

## `tako typegen`

Generate typed runtime and secret accessors.

```bash
tako typegen
```

JavaScript/TypeScript projects get `tako.gen.ts`. It exports runtime state, a typed `Secrets` interface, and helpers backed by `tako.sh/runtime`. Go projects get `tako_secrets.go`.

If a JS/TS project already has `channels/` or `workflows/`, typegen can scaffold missing demo files and add default exports where needed. It does not rewrite explicit channel names.

## `tako deploy`

Build and deploy an app to one environment.

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

`--env` defaults to `production`. Production deploys require confirmation unless `--yes` or `-y` is provided. `development` is reserved for `tako dev`.

Deploy validates config, builds locally, uploads artifacts, prepares releases on each server, runs the release command on the leader when configured, and performs rolling update.

## `tako logs`

View remote app logs.

```bash
tako logs
tako logs --env staging
tako logs --days 7
tako logs --tail
tako logs --json
```

`--env` defaults to `production`. `--days` defaults to `3` and applies to timestamped app log-file lines. `--tail` streams continuously and conflicts with `--days`. History mode opens in `$PAGER` when interactive, with `less -R` as the default. Diff-only pagers such as `delta` fall back to `less -R`; piped output and `--json` write to stdout.

Logs include app stdout/stderr and app-scoped Tako server diagnostics from the app log files. JS/TS production HTTP entrypoints route `console.*`, uncaught exceptions, and unhandled rejections into the same app log stream. Human-readable output formats log-file lines into timestamp, level, source, and message columns, with app process sources shown as `{instance}`, app-scoped server diagnostics shown as `tako`, and app stderr shown as `ERROR`; structured multiline logs, `fields.error.stack` values, and raw stderr object dumps stay inside one indented entry. Consecutive identical messages collapse to `└─ repeated N times through <timestamp>`; same-day repeats show `HH:MM:SS`, and `N` includes the first displayed row. Interactive terminals colorize levels and scopes, use the app runtime color for app process sources when known, and render trailing metadata fields such as `instance=...` as dim italic text. Remote fetch/connect failures are reported instead of being shown as empty logs.

`--json` emits JSONL for agents and automation. Each stdout line is one log event; structured app/worker JSON records are preserved with `source` and `instance_id`, while raw app process output and app-scoped Tako diagnostics are wrapped into JSON records with a `level` field. `source` is the app instance id, worker name (`default` renders as `worker`), or `tako`; records include `server` only when logs come from multiple servers.

## `tako releases`

List or roll back deployed releases.

```bash
tako releases ls
tako releases ls --env staging
tako releases rollback abc1234
tako releases rollback abc1234 --env staging --yes
```

`ls` shows newest releases first and marks the current release. `rollback` performs the standard rolling-update flow using the selected release, current routes, env, secrets, and desired scaling state.

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

Desired counts are stored on the server and persist across deploys, rollbacks, and server restarts.

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

Interactive mode can discover targets and prompt. Non-interactive mode requires `--yes`, `--env`, and `--server`. `development` is reserved for `tako dev`.

Delete sends the deployment id `{app}/{env}` to `tako-server`, drains the app, removes routes, and deletes `/opt/tako/apps/{app}/{env}`. Re-running delete for an absent target is safe.

## `tako secrets`

Manage encrypted project secrets.

```bash
tako secrets set DATABASE_URL
tako secrets set DATABASE_URL --env staging
tako secrets set API_KEY --env development --sync
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

Secret values are read from an interactive password prompt or stdin. If a secret already exists in the selected environment, interactive runs ask before overwriting it. `sync` sends local encrypted secrets to mapped servers after decrypting them locally.

Remote secret updates do not write `.env` files. `tako-server` stores secrets encrypted in SQLite, then restarts workflow workers and rolls HTTP instances so fresh long-running processes receive secrets through fd 3.

When `set` or `key export` omit `--env` in an interactive terminal, Tako opens an environment picker with `development`, `production`, existing environments, and `New environment`. Non-interactive runs must pass `--env`.

### Secret Keys

```bash
tako secrets key export
tako secrets key export --env staging
tako secrets key import
tako secrets key import --env production
tako secrets key import --passphrase --env production
```

The first secret set for an environment creates a random environment key. By default keys are stored under Tako's data directory at `keys/{key_id}`.

On macOS, interactive key creation and import offer iCloud Keychain storage through the signed `Tako.app` CLI installed by the macOS installer. If a local key file exists and iCloud Keychain is unavailable during a read, Tako uses the local key file. If the signed app entitlement is unavailable while saving to iCloud Keychain, Tako fails before writing a local key file or updating `.tako/secrets.json`.

`export` requires macOS user authentication when reading from Keychain, then copies a single base64url key string to the clipboard. `import` asks for a key source interactively: `Exported key` or `Passphrase`. If an existing environment's secrets cannot be decrypted with the passphrase, interactive mode prompts again with `Invalid passphrase`; non-interactive mode fails without saving the key. In non-interactive mode, `import --env <environment>` reads an exported key from stdin by default; pass `--passphrase --env <environment>` to import a passphrase.

## `tako servers`

Manage global server inventory.

```bash
tako servers add
tako servers add la
tako servers add la.tailnet.ts.net --description "Los Angeles" --port 22
tako servers add root@la
tako servers add la --install
tako servers add la --install --admin-user root
```

Without `host`, `add` opens an interactive setup wizard. With `host`, Tako defaults the local server name to the host's first DNS label; pass `--name` to override it. IP addresses and hosts that do not produce a valid name require `--name`. Use the server's Tailscale MagicDNS name or Tailscale IP. Normal add verifies Tailscale resolution, `tako@host` SSH recovery access, target metadata (`arch`, `libc`), and signed private management HTTP before writing `config.toml`. If a local SSH key is encrypted, interactive runs prompt for the passphrase; pass `--ssh-passphrase <PASSPHRASE>` for one-line commands.

Use `--install` to install or repair `tako-server` over SSH before adding the host. The admin user defaults to `root`; pass `--admin-user <user>` when the host requires a different privileged SSH user. `user@host` is shorthand for setting that admin SSH user during first add. The installer enrolls the SSH key used for access for both `tako` SSH recovery and signed HTTP management.

Other server commands:

```bash
tako servers ls
tako servers list
tako servers rm la
tako servers remove la
tako servers delete la
tako servers status
tako servers info
tako servers restart la
tako servers restart la --force
tako servers upgrade
tako servers upgrade la
tako servers setup-wildcard
tako servers setup-wildcard --env production
tako servers implode la
tako servers implode la --yes
```

`servers status` reads all configured servers through signed Tailscale HTTP management and prints a snapshot of deployed apps. It can run from any directory.

`servers restart` performs a zero-downtime service-manager reload by default. `--force` performs a full restart. `servers upgrade` installs a new `tako-server` binary and reloads through the management socket handoff. GitHub-backed server upgrade metadata and archive downloads use `GH_TOKEN` when set, falling back to `GITHUB_TOKEN`.

`servers setup-wildcard` configures DNS-01 wildcard certificate support on mapped servers. `servers implode` removes `tako-server`, server-side data, services, sockets, and the local server entry.

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

This removes local config/data, installed CLI binaries, and system-level local dev setup such as DNS, loopback redirects, trust-store entries, and launchd or systemd helper configuration.
