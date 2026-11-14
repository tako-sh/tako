---
title: CLI Reference
description: "Look up Tako command groups, global options, environment selection, and structured output for local development, deployment, and operations."
---

```bash
tako [--version] [-v|--verbose] [--ci] [--json] [--dry-run] [-c|--config <CONFIG>] [--ssh-passphrase <PASSPHRASE>] <command>
```

Progress, prompts, and diagnostics normally go to stderr. Command results and machine-readable data go to stdout. Interactive `tako dev` owns stdout for its live interface and logs; `tako run` preserves child stdout.

Use `tako <command> --help` or the connected MCP tool's schema for the exact arguments supported by your installed CLI. This reference explains command groups and behavior; task pages cover complete workflows.

## Global Options

| Option                          | Meaning                                                                                                                                |
| ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `--version`                     | Print the CLI version and exit.                                                                                                        |
| `-v`, `--verbose`               | Append-only transcript with timestamps, log levels, and debug diagnostics.                                                             |
| `--ci`                          | Deterministic non-interactive output: no colors, spinners, raw mode, or prompts.                                                       |
| `--json`                        | Emit structured JSON on stdout. Progress, diagnostics, and errors stay on stderr.                                                      |
| `--dry-run`                     | Preview supported changes. A command that cannot preview its changes exits with an error before running.                               |
| `-c`, `--config <CONFIG>`       | Use an explicit config file. `.toml` is appended when omitted.                                                                         |
| `--ssh-passphrase <PASSPHRASE>` | Use a passphrase for encrypted local SSH keys.                                                                                         |

App-scoped commands that honor `-c`: `agents`, `mcp`, `init`, `dev`, `run`, `logs`, `deploy`, `releases`, `backups`, `delete`, `secrets`, `credentials`, `storages`, `generate`, and project-context `scale`.

Preview changes with `--dry-run` on `agents`, `deploy`, `scale`, `delete`, `connect`, `disconnect`, `ssh`, `secrets sync`, `secrets migrate`, `backups now/download/restore`, and `workflows retry/cancel`. Network membership commands also support previews, including server add, remove, drain, resume, and uninstall. Read-only commands still report their results.

The config file's parent directory determines the project root and local `.tako/` state. Config files in the same directory share secret drafts; a bound environment cannot be reused for another app or network. See [alternate configurations](/secrets/#use-an-alternate-configuration).

For finite commands, `--json` prints one final object. Commands without a specialized schema use `{"ok":true,"command":"<command>"}`. Failures print `{"ok":false,"error":{"message":"..."}}` on stdout and the human-readable error on stderr. Specialized objects: `status` includes `servers`; `doctor` includes paths, CA, dev server, apps, and proxy checks; `network server list`, `secrets list`, `credentials list`, `dev list`, `backups list`/`status`, and `releases list` include their collections; `backups download` and `backups restore` include the backup entry and what they wrote. Streaming exceptions keep stdout as JSONL until interrupted: `tako logs --tail --json` emits one log record per line, and `tako dev --json` emits a ready object, then log and event records, with no logo or TUI. `tako run` is also an exception: child stdout stays untouched and no JSON result object is appended.

Installed CLIs send an anonymous event per command so we can count unique users and see which commands run. Set `TAKO_TELEMETRY=0` to opt out. See [Usage stats](#usage-stats).

## Project Setup

```bash
tako init
```

Creates `tako.toml`, updates `.gitignore`, detects runtime and preset, asks for app name and production route, and installs the SDK package for JS and Go projects. If the production route is a wildcard route, init can collect and encrypt the Cloudflare credential required for Let's Encrypt DNS-01. Interactive init then offers coding agent setup with a default of yes; non-interactive init leaves agent settings alone.

```bash
tako generate
tako gen
tako g
```

Refreshes generated project files:

- JS/TS: `tako.d.ts` with environment names, secret names, storage bindings, channel metadata, typed Store params and data, workflow metadata, and env var names.
- Go: `tako_secrets.go` with typed secret accessors.

For JS/TS, generation defaults to the project root and keeps an existing `tako.d.ts` in `app/`, `src/`, or the project root. Empty existing `channels/` or `workflows/` directories get demo definitions. Declaration files are never scaffolded as handlers.

Generation exports the app's language-neutral `tako-schema.json` beside `tako.toml`. Root-level `schemas` lists local files or HTTP(S) URLs to import. JavaScript gets generated `tako.imports.ts` and convenience `tako.exports.d.ts` beside its existing `tako.d.ts`; Go gets `tako_imports.go`, and Rust gets `tako_imports.rs`. Native outputs use `build.cwd` or project root. Native exporters use `tako-schema/main.go` or `examples/tako_schema.rs`; see [Network Primitives](/network-primitives/) for DTO setup and runtime limits. No package exports are edited.

Remote schemas are pinned, with their content, in `tako-schema.lock`. Commit it for reproducible offline generation. `tako gen --update-schemas` fetches remote sources again; ordinary generation reuses the lock and rereads local paths. Handwritten `tako.imports.ts` files must migrate to `schemas` before generation can replace them.

## `tako agents`

```bash
tako agents
tako agents --agent codex,claude-code --yes
tako agents --agent cursor --dry-run --json
```

Installs bundled workflow and SDK skills and configures the local MCP server for
Codex, Claude Code, Cursor, and VS Code Copilot. Detects clients by default;
`--agent` selects them explicitly (repeatable or comma-separated). `--yes` skips
the setup confirmation. `--dry-run` previews files without writing. All settings
are project-local; existing unrelated configuration is preserved. Re-run to
refresh after upgrading Tako. See [Coding agents](/agents/).

## `tako mcp`

```bash
tako mcp
tako mcp -c /path/to/project/tako.toml
```

Serves Tako tools, bundled docs, and SDK skills over MCP stdio. Designed to be
started by a coding agent; stdout is reserved for MCP protocol messages,
including with `--json`. Tools use existing CLI credentials and permissions.
See [Coding agents](/agents/) for manual client configuration and tool behavior.

## `tako dev`

```bash
tako dev
tako dev vite dev
tako dev --variant preview
tako dev --var preview
tako dev --tunnel
tako dev --restart
tako dev --background --json
```

Starts or attaches to a local dev session behind trusted HTTPS and real hostnames. `--variant foo` uses a variant hostname such as `myapp-foo.test`; `--restart` restarts the process with the current config instead of attaching. It starts the dev daemon, prepares DNS/proxy/CA setup, generates files, injects secrets and storage through fd 3, waits for fd-4 readiness, and registers routes.

Pass a command after `tako dev` to override the configured dev command for that run:

```bash
tako dev vite dev
tako dev npm run dev
tako dev -- vite dev
```

Use `--` when the child command conflicts with a Tako subcommand such as `stop` or `list`, or when the child command starts with a flag.

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

`--background` exits after registration and readiness while the dev daemon keeps
the app running. Combine it with `--json` for one ready object, then inspect
with `tako dev list` and stop with `tako dev stop`. Global `--json` skips the interactive UI. `tako dev --json` prints a ready object with `app`, `url`, and `hosts`, then JSONL `log` and `event` records. `tako dev list --json` prints one object with an `apps` array.

## `tako doctor`

```bash
tako doctor
```

Prints local diagnostics for the dev daemon, local DNS, TLS files, and platform-specific proxy setup. Global `--json` emits one object with `paths`, `ca`, `dev_server`, `apps`, and platform `proxy` checks instead of the human report.

## `tako run`

```bash
tako run scripts/foo.ts
tako run jobs/foo.go --dry
tako run --eval 'console.log(tako.env)' -- --dry
tako run --env staging scripts/foo.ts
tako run -- cargo run --bin migrate
```

Runs a one-off command locally from the app directory with Tako project context. `--env` defaults to `development`.

Script files are expanded by the selected runtime. Bun runs JS/TS scripts with Bun, Node runs TS scripts with type stripping, and Go runs `.go` files with `go run`. `--eval` runs inline source when the runtime supports it; JS runtimes support inline TypeScript. Use `-- {command...}` for exact commands and runtimes without a bare script rule.

The child process receives `[vars]` plus `[vars.<env>]`, `ENV`, `TAKO_BUILD=local`, `TAKO_DATA_DIR`, runtime defaults, and `TAKO_APP_ROOT` for JS apps. Tako decrypts local app secrets for the selected environment and passes the normal bootstrap envelope through `TAKO_BOOTSTRAP_DATA`. SDK-aware scripts use the same app SDK surfaces; JS/TS scripts can import `tako` and use `tako.secrets` and `tako.storages`.

Secrets are not process env vars. `tako run` is local-only in v0; it does not run commands on deployed servers.

## Deploy And Runtime Operations

```bash
tako deploy [--env <env>] [-y|--yes] [--force] [--description <text>]
```

Builds locally, uploads artifacts over signed HTTP management, prepares every mapped server, runs the optional release command on the leader, rolls instances, finalizes the release, and runs post-deploy backups when enabled. When an hourly database snapshot is running, the post-deploy backup is queued on the server and the deploy reports `backup queued` instead of waiting; a failed post-deploy backup is reported without failing the deploy. `--env` defaults to `production`. `development` is reserved for `tako dev`. Deploy requires the CLI, server, and release artifact to use the same protocol version; a mismatch fails before release commands, workflows, or app processes start. `--force` attempts the deploy despite a mismatch but does not skip normal validation or readiness checks. It is separate from `--yes`, which only skips confirmation.

```bash
tako logs [--env <env>] [--tail] [--days N]
```

Shows app logs from all servers mapped to the environment. History defaults to three days. `--tail` streams. Global `--json` emits a final object with a `logs` array in history mode; with `--tail`, it emits JSONL records.

```bash
tako scale <instances> [--service <service>] [--env <env>] [--server <server>] [--app <app>]
tako scale --reset [--service <service>] [--env <env>] [--server <server>] [--app <app>]
```

Changes desired instance count on targeted servers. From a project, app identity is resolved from the config. Outside a project, pass `--app`, either as a base app with `--env` or as a full `{app}/{env}` deployment id.

For Compose, the target defaults to `[compose].service`; `--service` selects another service. The count is a persistent override that survives deploys, rollbacks, and restarts. `--reset` clears the selected service override and applies its current Compose `deploy.replicas` default (one when omitted). Supply either a count or `--reset`. These flags require a Compose app. See [Compose scaling](/docker-compose/#scale-individual-services) for limits and shared-volume behavior.

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

Release descriptions default to the commit subject for clean checkouts, `Uncommitted changes based on: <commit subject>` for dirty checkouts, or `Local source snapshot` without Git history. Set `tako deploy --description "Fix reconnect handling"` to override the description. The dirty status remains visible with a custom description. `releases list --json` includes `description`, `commit_message`, `git_dirty`, and `sdk` for each release. `sdk` is `{ "language", "name", "version" }` for the Tako SDK detected at deploy time, or `null` when the release has none.

Lists release/build history or rolls an environment back to a previous release. Rollback uses the standard rolling-update flow and reports partial server failures individually.

## Networks

```bash
tako network create <name>
tako network client add [--network <id-or-name>] [--admin | --deploy <app/env>] [--ssh] [--expires-in <seconds>]
tako network client list [--network <id-or-name>]
tako network client remove <device> [--network <id-or-name>]
tako network join [--name <device>] [--invitation-stdin]
tako network sync [network]
tako network request [--name <device>] [--network <id-or-name>] --output <file>
tako network approve <request-file> [--network <id-or-name>] --output <file> [--deploy <app/env>] [--admin] [--ssh] [--expires-in <seconds>]
tako network import <profile-file>
tako network export [network] --output <file>
tako network list --json
tako network status [network] --json
tako network revoke <device> [--network <id-or-name>]
tako network server drain <server> [--network <id-or-name>] [--timeout <seconds>]
tako network server resume <server> [--network <id-or-name>]
tako connect [network]
tako disconnect [network]
```

Invitations are single-use, expire after 3600 seconds by default (`client add --expires-in` accepts 60–86400), and are redeemed through their issuing server. `join` prompts for the invitation or reads stdin. `network sync` pulls published ciphertext without publishing worktree drafts. Network names are shared and currently immutable.

Each device generates and retains its own keys. Repeat `--deploy` for exact app/environment scopes; `--ssh` requires `--admin`. `network approve --expires-in` accepts `1..31536000` seconds and limits membership lifetime, separately from the request's activation window. Import requires the original requesting device's keys. Export contains public network configuration, not private keys or app secrets.

`drain` and `resume` require server administration and a unique member name or device ID. Drain defaults to 120 seconds and accepts `0..3600`; zero requests immediate force. Inspect the structured completion report, not only the drained intent. See [Native server maintenance](/operations/#native-server-maintenance).

`ssh` uses embedded WireGuard and userspace TCP without sudo or system network changes; OpenSSH is required. `connect` enables local applications, private DNS, short server names, and approved SSH aliases such as `ssh edge-a`. It explains the required administrator access before prompting. `disconnect` removes those managed settings. No external WireGuard executable is required. Normal native management uses pinned HTTPS without a tunnel or SSH agent. See [Native networks](/networks/) for platform requirements, enrollment, endpoints, and private service names.

## Server administrator recovery

Run on a surviving Linux server through its provider console or administrator SSH:

```bash
sudo tako-server admin invite [--expires-in <seconds>] [--ssh] [--json]
```

Requires root and a running native network agent with intact state and keys. Prints a single-use invitation for an independent administrator, with a default validity of 3600 seconds (60–86400 accepted). Join from the replacement machine with `tako network join`. `--ssh` also grants a Tako administrator shell. Ordinary client invitations remain delegated. See [Recover administrator access](/networks/#recover-administrator-access).

## Servers

```bash
tako network server add [admin@host] [--network <id-or-name>] [--name <name>] [--wireguard-endpoint <ip:udp-port>] [--port <ssh-port>] [--ssh-key <path>] [--http-port <port>] [--https-port <port>]
tako network server list
tako network server ls
tako status
tako network server reload <name> [--force]
tako network server upgrade [name] [--force]
tako network server remove [name]
tako network server rm [name]
tako network server uninstall [name] [-y|--yes]
```

`network server add` bootstraps over administrator SSH and enrolls the server's own keys, HTTPS pins, and endpoint in a Tako network. No `[[servers]]` entry is written. Without `--network`, selection uses the project's production network (honoring `--config`) or the sole configured network. An explicit `--network` takes precedence. Create or import a network first; ambiguous selection requires `--network`.

Without a host, an interactive terminal prompts for `admin@host`. An omitted SSH user defaults to `root`. Public HTTP and HTTPS ports default to `80` and `443`. `--ssh-key` selects a private key for bootstrap and saves its absolute path locally for SSH maintenance (reload, upgrade, uninstall). This selection stays on the workstation; normal HTTPS management uses the enrolled device identity. External VPN providers and the old `--vpn`, `--install`, `--no-test`, and server `--description` options are removed. See [Servers](/servers/#choose-the-vpn-provider) to replace an old setup.

`status` prints a deployment snapshot grouped by server, with compact server summary, routes, and app rows. An app whose instances the server had to replace shows a `Restarts` row with the restart count, the last exit (crashed, unhealthy, or failed to start, with its exit code or signal), and when the next restart is due while restarts are backing off. `--json` reports the same under `recovery` in each app status.

Use the exact selector shown by `network server list --json` for maintenance commands. Usually `network-name/server-name`, it includes network or server IDs when names collide; see [Server maintenance](/servers/#server-maintenance). `network server reload` performs a zero-downtime service reload by default. Its `--force` flag performs a full restart. `network server upgrade` checks the candidate protocol against the CLI and every active app before reload, checks again after readiness, and restores and restarts the previous binary if the upgrade fails. `network server upgrade --force` attempts an incompatible upgrade but keeps readiness and rollback checks enabled. `network server uninstall` publishes a revocation of native membership and delegated approvals before removing the remote service, binaries, data, and sockets. Revocation failure stops uninstall before deletion; cleanup failure after revocation reports uninstall as incomplete. Offline peers enforce the revocation when they reconnect. `--dry-run` performs neither revocation nor deletion. `network server remove` revokes network membership and delegated approvals without deleting app data.

## Secrets

```bash
tako secrets set <NAME> [--env <env>] [--expires-on <when>]
tako secrets add <NAME> ...
tako secrets rm <NAME> [--env <env>] [--yes]
tako secrets list
tako secrets sync [--env <env>] [--retry-expired]
tako secrets migrate
tako secrets key export [--env <env>]
tako secrets key import [--env <env>] [--passphrase]
```

Secrets are app-facing values delivered to SDKs. Values are encrypted in ignored worktree-local `.tako/secrets.local.json`; keys are stored outside the project. Interactive prompts accept pasted multiline values and ask before trimming surrounding whitespace; non-interactive commands read the full stdin stream. Set/remove commands only edit local drafts. Deploy and `secrets sync` pull published state, preserve local edits, and publish per-key changes for the selected environment. Sync does not restart running apps; deploy to apply values. Network-bound secrets, provider credentials, storage credentials, and backup keys are replicated as ciphertext between servers. Authorized clients cache them through HTTPS.

Secret state retains the current value per key and versioned deletions. Pending publication requests expire after seven days; `--retry-expired` explicitly replaces expired requests with the current local drafts as new changes. Review the drafts first, since they can overwrite remote changes to those keys.

`secrets migrate` validates the original environment keys, writes local state, and removes the old `.tako/secrets.json` after retaining an ignored recovery copy. Commit the tracked file's deletion. Historical commits remain unchanged. See [Secrets](/secrets/) for conflicts, worktrees, and recovery.

`secrets key import` can recover the original environment key before local drafts are merged with cached shared values. It accepts an exported key through a masked prompt or stdin; use `--passphrase --env <env>` to derive the original key from its passphrase instead. It rejects keys that cannot decrypt existing values. An invitation restores access to published values, but does not recover unpublished drafts or legacy environment keys.

Expiry accepts `YYYY-MM-DD` or `in N days`. Leave it blank for no expiry. Deploy fails on expired selected-environment secrets and warns when they expire within 30 days.

## Provider Credentials

```bash
tako credentials set [name] [--env <env>] [--expires-on <when>]
tako creds set ...
tako credentials rm <name> [--env <env>]
tako credentials list
```

Running `tako credentials` without a subcommand lists credentials. Set and remove prompt for an environment when `--env` is omitted; non-interactive use requires `--env`.

Provider credentials are Tako-owned runtime credentials, not app secrets. Supported names are `ssl.cloudflare` and `postgres_url`. They are encrypted in local state and shared network records, never exposed to app code, and published by `tako secrets sync` or deploy. Running processes receive the new bindings on deploy.

`ssl.cloudflare` powers Let's Encrypt DNS-01 and Cloudflare Origin CA. `postgres_url` enables shared channel replay, Presence membership, Store invalidation, and workflow state for multi-server deployments. It is a provider credential and is not exposed to app code.

## Storage

```bash
tako storages add <binding> [--env <env>] [--resource <resource>] [--provider s3|local] [--bucket <bucket>] [--endpoint <https-url>] [--region <region>] [--access-key-id <value>] [--secret-access-key <value>] [--expires-on <when>] [--force-path-style] [--public-base-url <https-url>]
tako storages credentials <resource> [--env <env>] [--access-key-id <value>] [--secret-access-key <value>] [--expires-on <when>]
```

Storage commands default to `--env production` and `--provider s3`.

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

`backups list` shows each entry's kind: `full` for an archive of the whole data tree, or `databases` for an hourly snapshot of only its SQLite databases. `--json` entries include `kind` and, for a database snapshot, its `databases`. `backups status` reports the latest full backup, the latest database snapshot, the reason the latest hourly database snapshot was skipped, if it was, and why scheduled full backups are failing, with the next attempt (`--json`: `last_backup`, `last_database_snapshot_at_unix_secs`, `last_database_snapshot_skip`, `last_backup_failure`). `download` saves the encrypted archive object, by default as `<backup-id>.tar.zst.enc` for a full backup or `<backup-id>.databases.tar.zst.enc` for a database snapshot, a name built from the validated ID; `--json` prints `backup`, `path`, and `size_bytes`. `restore` of a full backup replaces the data directory; restore of a database snapshot replaces only the SQLite databases it contains, removes their `-wal`, `-shm`, and `-journal` files, and leaves other files untouched. The confirmation names what the selected kind replaces, including the workflow-state rewind when the snapshot contains `tako/workflows.sqlite`. `restore --json` prints `backup` and, for a database snapshot, `databases` with the `replaced`, `created`, and `untouched` paths.

## Workflows

```bash
tako workflows list [--env <env>] [--server <server>] [--status <status>] [--name <workflow>] [--limit <n>]
tako workflows ls ...
tako workflows show <run-id> [--env <env>] [--server <server>]
tako workflows retry <run-id> [--env <env>] [--server <server>]
tako workflows cancel <run-id> [--env <env>] [--server <server>] [-y|--yes]
```

Workflow commands inspect and repair the durable runs of a deployed environment. They default to `production`, require project context, and target the deployment id `{app}/{env}` on every mapped server, or only on `--server`. They are not available for `development`; use the `tako dev` logs locally.

`list` shows the newest runs first: ID, workflow, status, attempts used of the retry budget, the next due time of a pending run or the finish time of a finished one, and the first line of the last error. `--status` accepts `pending`, `running`, `succeeded`, `cancelled`, or `dead`. `--name` selects one workflow. `--limit` accepts `1..200` and defaults to 50. `show` prints every run field, the payload and result, up to the 100 most recent saved steps oldest first, event waits, and the stored error. Servers cap large values; a capped value shows the start of its JSON text and its stored size.

`retry` accepts a `dead` or `cancelled` run. The run becomes `pending`, due now, with a fresh retry budget; saved steps are kept, so it resumes after its last checkpoint. `cancel` accepts a `pending` or `running` run and asks for confirmation in an interactive terminal unless `--yes` is passed. A running handler is not interrupted: it stops at its next checkpoint (the next step, sleep, or wait) and cannot complete the run; side effects already started are not undone. Retry does not prompt. Both act on exactly one server.

Without `postgres_url`, each server keeps its own runs, so results are labelled with their server; if two servers hold a run with the same ID, `show`, `retry`, and `cancel` fail and ask for `--server`. With `postgres_url`, every server shares one store: runs are listed once, without a server, and retry or cancel is sent once. A server that cannot be reached is reported as a warning while the others answer; the command fails only when no server answers, or when the run is not found on any server.

Global `--json` prints one object. Every object has `ok`, `command: "workflows"`, `app`, `environment`, `store` (`"sqlite"` or `"postgres"`), and `errors`, an array of `{name, error}` for servers that did not answer.

- `list` adds `runs` and `has_more`. Each run has `id`, `name`, `status`, `attempts`, `max_attempts`, `created_at_ms`, `run_at_ms` (null while waiting for a signal), `finished_at_ms`, `last_error` (capped at 1 KiB), `last_error_truncated`, and `server` (null with Postgres).
- `show` adds `server` and `run`. `run` has the list fields plus `lease_until_ms`, `worker_id`, `payload`, `result` (null until the run succeeds), `steps` (`name`, `completed_at_ms`, `result`), `steps_total`, and `waits` (`step_name`, `event_name`, `expires_at_ms`). Each value is `{value, truncated, bytes, preview?}`; a truncated value has `value: null` and a `preview` of its JSON text.
- `retry` and `cancel` add `server`, `id`, `previous_status`, and `status`.

Failures print `{"ok": false, "error": {"message": ...}}`, as for every command.

## Local CLI Maintenance

```bash
tako completions [-y|--yes]
tako upgrade
tako uninstall [-y|--yes]
tako version
```

`completions` installs bash, zsh, and fish completion scripts for shells that are on `PATH`. zsh is installed only when a writable directory is already on `fpath`; otherwise Tako prints how to add one. `upgrade` updates the local CLI install. On macOS, official CLI upgrades support Apple Silicon and Intel. Intel builds publish separately after each release. If an Intel download is temporarily unavailable, retry once that build finishes. `uninstall` removes local Tako binaries, local data, completion scripts, and platform-specific dev services/config after confirmation.

## Usage stats

Official CLI installs send one anonymous event per command so we can count unique users and see which commands run (`deploy`, `dev`, `init`, and so on). The event includes CLI version, OS, architecture, and the command name. It does not include app names, paths, server hosts, environment names, or command arguments. `tako-server` does not send usage stats.

Opt out:

```bash
export TAKO_TELEMETRY=0
```

Usage stats are always off during dry runs and in CI (`CI=true` or `--ci`), even with `TAKO_TELEMETRY=1`. Local Cargo builds are off by default; set `TAKO_TELEMETRY=1` to send from local builds outside CI.
