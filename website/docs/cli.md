---
title: CLI Reference
description: "Look up Tako command groups, global options, environment selection, and structured output for local development, deployment, and operations."
---

```bash
tako [--version] [-v|--verbose] [--ci] [--json] [--dry-run] [-c|--config <CONFIG>] [--ssh-passphrase <PASSPHRASE>] [--network <NETWORK>] <command>
```

Progress, prompts, and diagnostics normally go to stderr. Command results and machine-readable data go to stdout. Interactive `tako dev` owns stdout for its live interface and logs; `tako run` preserves child stdout.

Use `tako <command> --help` or the connected MCP tool's schema for the exact arguments supported by your installed CLI. This reference explains command groups and behavior; task pages cover complete workflows.

## Global Options

| Option                          | Meaning                                                                                                  |
| ------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `--version`                     | Print the CLI version and exit.                                                                          |
| `-v`, `--verbose`               | Append-only transcript with timestamps, log levels, and debug diagnostics.                               |
| `--ci`                          | Deterministic non-interactive output: no colors, spinners, raw mode, or prompts.                         |
| `--json`                        | Emit structured JSON on stdout. Progress, diagnostics, and errors stay on stderr.                        |
| `--dry-run`                     | Preview supported changes. A command that cannot preview its changes exits with an error before running. |
| `-c`, `--config <CONFIG>`       | Use an explicit config file. `.toml` is appended when omitted.                                           |
| `--ssh-passphrase <PASSPHRASE>` | Use a passphrase for encrypted local SSH keys.                                                           |
| `--network <NETWORK>`           | Network to act on, by local alias or stable ID. See [Choosing a network](#choosing-a-network).           |

App-scoped commands that honor `-c`: `agents`, `mcp`, `init`, `dev`, `run`, `logs`, `deploy`, `release`, `backup`, `delete`, `secret`, `credential`, `storage`, `generate`, and project-context `scale`, `status`, and `doctor`.

Preview changes with `--dry-run` on `agents`, `deploy`, `scale`, `delete`, `connect`, `disconnect`, `ssh`, `secret sync`, `secret migrate`, `secret clear`, `backup now/download/restore`, and `workflow retry/cancel`. Network membership commands also support previews, including server add, revoke, remove, drain, and resume. Read-only commands still report their results.

The selected config file supplies environment networks in `[envs.<env>].network`; its parent directory determines the project root and local `.tako/` state. Config files in the same directory have independent network settings but share secret drafts; unpublished drafts cannot move to another app or network. See [alternate configurations](/secrets/#use-an-alternate-configuration).

For finite commands, `--json` prints one final object. Commands without a specialized schema use `{"ok":true,"command":"<command>"}`. Failures print `{"ok":false,"error":{"message":"..."}}` on stdout and the human-readable error on stderr. Specialized objects: `status` includes `servers` and, in a project, `project`; `doctor` includes paths, CA, dev server, apps, proxy checks, and network reachability; `network server list`, `secret list`, `credential list`, `dev list`, `backup list`/`status`, and `release list` include their collections; `backup download` and `backup restore` include the backup entry and what they wrote; `secret clear` includes `environment`, the `cleared` secret names, and `dry_run`. Streaming exceptions keep stdout as JSONL until interrupted: `tako logs --tail --json` emits one log record per line, and `tako dev --json` emits a ready object, then log and event records, with no logo or TUI. `tako run` is also an exception: child stdout stays untouched and no JSON result object is appended.

Installed CLIs send an anonymous event per command so we can count unique users and see which commands run. Set `TAKO_TELEMETRY=0` to opt out. See [Usage stats](#usage-stats).

### Choosing a network

Every command accepts the global `--network <alias-or-id>`, before or after the subcommand. It takes this machine's [alias](/networks/#name-networks-on-this-machine) for a network or its stable ID; the shared network name is not looked up. There is no default network.

Project commands pick an environment's network from `--network`, then `[envs.<env>].network` in the selected TOML config, then this machine's only network. Otherwise an interactive terminal asks, and without a terminal the command fails and lists this machine's aliases. Inside a project, pass `--env` and leave the network to the config. `deploy` records the network's stable ID in that config; with a `--network` that differs from the configured ID, it asks to move the environment and fails without a terminal. `secret set`, `rm`, and `sync` record a network only when the environment has none yet. Other uses of `--network` apply for that run only. `development` never has a network. See [Choose an environment's network](/networks/#choose-an-environments-network).

Network commands take the network as an argument or with `--network`, else use the only network, else ask in a terminal, else fail.

## Project Setup

```bash
tako init
```

Creates `tako.toml`, updates `.gitignore` so `.tako/` stays ignored, detects runtime and preset, asks for app name and production route, and installs the SDK package for JS and Go projects. If the production route is a wildcard route, init can collect and encrypt the Cloudflare credential required for Let's Encrypt DNS-01. Interactive init then offers coding agent setup with a default of yes; non-interactive init leaves agent settings alone.

```bash
tako generate
tako gen
tako g
```

Refreshes generated project files:

- JS/TS: `tako.d.ts` with environment names, secret names, storage bindings, channel metadata, typed Store params and data, workflow metadata, and env var names.
- Go: `tako_secrets.go` with typed secret accessors.

For JS/TS, generation writes to the app's package (the nearest directory with a `package.json` between `app_root` and `tako.toml`), defaulting to the project root, and keeps an existing `tako.d.ts` in that directory's `app/`, `src/`, or root. Empty existing `channels/` or `workflows/` directories get demo definitions. Declaration files are never scaffolded as handlers.

Generation exports the app's language-neutral `tako-schema.json` beside `tako.toml`, or in a JavaScript app's package; a JavaScript app with no network primitives or `schemas` gets no network files. Root-level `schemas` lists local files or HTTP(S) URLs to import. JavaScript gets generated `tako.imports.ts` and convenience `tako.exports.d.ts` beside its existing `tako.d.ts`; Go gets `tako_imports.go`, and Rust gets `tako_imports.rs`. Native outputs use `build.cwd` or project root. Native exporters use `tako-schema/main.go` or `examples/tako_schema.rs`; see [Network Primitives](/network-primitives/) for DTO setup and runtime limits. No package exports are edited.

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
tako dev --env test-e2e --background --json
```

Starts or attaches to a local dev session behind trusted HTTPS and real hostnames. `--variant foo` uses a variant hostname such as `myapp-foo.test`; `--restart` restarts the process with the current config instead of attaching. It starts the dev daemon, prepares DNS/proxy/CA setup, generates files, injects secrets and storage through fd 3, waits for fd-4 readiness, and registers routes.

`--env` defaults to `development`. A custom environment must be configured in
`[envs.<env>]`, `[vars.<env>]`, or Compose service `x-tako.envs.<env>`.
It selects variables, secrets, routes, storage,
and local database bindings, with independent registration and local data.
See [Local environments](/development/#local-environments).

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
tako dev stop --env test-e2e
tako dev list --env test-e2e --json
```

`stop` without a name stops the selected config/environment (`development` by default).
`--all` stops all registered dev apps; adding `--env` filters them. `list` shows
all environments unless filtered with `--env`, including each environment, config,
and active tunnel URL. JSON app records include `env` and `config`.

`--background` exits after registration and readiness while the dev daemon keeps
the app running. Combine it with `--json` for one ready object, then inspect
with `tako dev list` and stop with `tako dev stop`. Global `--json` skips the interactive UI. `tako dev --json` prints a ready object with `app`, `url`, and `hosts`, then JSONL `log` and `event` records. `tako dev list --json` prints one object with an `apps` array.

## `tako doctor`

```bash
tako doctor
```

Prints local diagnostics for the dev daemon, local DNS, TLS files, and platform-specific proxy setup. For each local network, it completes a WireGuard handshake with every server and reports whether the UDP endpoint is reachable, gets no reply, or is refused. A server is not checked while `tako connect` or a `tako ssh` session for that network is active or starting, because a probe would move the server's peer endpoint away from that transport. Global `--json` emits one object with `paths`, `ca`, `dev_server`, `apps`, platform `proxy` checks, and `networks` instead of the human report. An unreadable local network index appears as one `networks` entry with `status: "error"` and a `message`. Each server in `networks` has `status` (`reachable`, `no_reply`, `refused`, `skipped`, or `error`), `wireguard_endpoint`, `rtt_ms`, and `message`.

Inside a project, doctor adds a `Project` section of checks: a route that needs `ssl.cloudflare` (a wildcard Let's Encrypt route, or the Cloudflare provider) without it set; secrets and credentials that expired or expire within 30 days; a database bound in `tako.toml` with no record in the network state; and a target server that hasn't yet received key grants for the environment's current secret records. Each check reads local state or makes one network request. A check that needs an unreachable network, or secrets a deploy client can't read, is skipped with a note. `--json` adds `project`: `{"app", "checks": [{"name", "status": "ok"|"warning"|"skipped", "message"}]}` with check names `wildcard_ssl`, `expiring_values`, `database_records` and `key_grants`, or `null` outside a project.

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

The child process receives `[vars]` plus `[vars.<env>]`, `ENV`, `TAKO_BUILD=local`, `TAKO_DATA_DIR`, runtime defaults, and `TAKO_APP_ROOT` for JS apps. Tako decrypts local app secrets for the selected environment and passes the normal bootstrap envelope through `TAKO_BOOTSTRAP_DATA`. SDK-aware scripts use the same app SDK surfaces; JS/TS scripts can import `tako` and use `tako.secrets`, `tako.storages`, and `tako.databases`. Development database bindings are served from local files under `.tako/data/databases/`. Other environments get no databases, with a warning: `tako run` never connects to hosted databases.

Secrets are not process env vars. `tako run` is local-only in v0; it does not run commands on deployed servers.

## Deploy And Runtime Operations

```bash
tako deploy [--env <env>] [-y|--yes] [--force] [--description <text>]
tako deploy --env production --dry-run --json
```

Uses the environment's [network](#choosing-a-network) and records its ID in `[envs.<env>].network` in the selected TOML config; in an interactive terminal it offers to add a server when that network has none yet. Builds locally, uploads artifacts over signed HTTP management, prepares every mapped server, runs the optional release command on the leader, rolls instances, and finalizes the release. Before the release command, it snapshots each bound [hosted database](/databases/) that changed and has backups configured; deploys do not create full-directory archives. `--env` defaults to `production`. `development` is reserved for `tako dev`. Deploy requires the CLI, server, and release artifact to use the same protocol version; a mismatch fails before release commands, workflows, or app processes start. `--force` attempts the deploy despite a mismatch but does not skip normal validation or readiness checks. It is separate from `--yes`, which only skips confirmation.

On a network with exactly one trusted server, a full client creates missing declared app SQLite databases and, for a channel-capable release without a selected backend, the generated replay database. It reports each new name and home once. A successful `--json` deploy lists these in `created_databases` (`name`, `server`); an empty list means it created none. With several trusted servers, use `tako sqlite create <name> --server <server>` to place a missing database; for replay, set `[envs.<env>.channels].database` to that name. A restricted deploy client needs a full client to provision database access first. See [Deploy Replay Storage](/channels/#deploy-replay-storage).

`--dry-run --json` previews database creations (`create_databases`) and the selected replay database (`channel_database`, with `name`, `server`, and `implicit` when present) from cached network state. `channel_replay_unprovisioned` is true when a deploy-only client finds no replay database to use. It does not create anything, grant access, build, or deploy. In particular, a cached plan is not confirmation that every server currently has the required grants or that its database is reachable.

```bash
tako logs [--env <env>] [--tail] [--days N]
```

Shows app logs from all servers mapped to the environment. History defaults to three days. `--tail` streams. Global `--json` emits a final object with a `logs` array in history mode; with `--tail`, it emits JSONL records.

```bash
tako scale <instances> [--service <service>] [--env <env>] [--server <server>] [--app <app>]
tako scale --reset [--service <service>] [--env <env>] [--server <server>] [--app <app>]
```

Changes desired instance count on targeted servers. From a project, app identity is resolved from the config. Outside a project, pass `--app`, either as a base app with `--env` or as a full `{app}/{env}` deployment id.

For a Compose group with exactly one HTTP app, that app is the default scale target. With multiple HTTP apps, supply `--service`; SDK workers and untagged Docker services are always selected with `--service`. The count is a persistent override that survives deploys, rollbacks, and restarts. `--reset` clears the selected service override and applies its current Compose `deploy.replicas` default (one when omitted). Supply either a count or `--reset`. These flags require a Compose app. Scaling a stopped app or group updates its count for the next start without resuming it. See [Compose scaling](/docker-compose/#scale-individual-services) for limits and shared-volume behavior.

```bash
tako stop [--service <service>] [--env <env>] [--server <server>] [--app <app>]
tako start [--service <service>] [--env <env>] [--server <server>] [--app <app>]
```

Stops or starts an existing deployment. From a project, pass `--env` to use its configured servers, or select `--server` (defaulting to the production environment when `--env` is omitted). Outside a project, pass `--server` and either `--app <app>/<env>` or `--app <app> --env <env>`. Both commands support global `--dry-run` and `--json`.

For Compose, `--service` selects an `x-tako` service, including SDK workers; omitting it selects the whole group, including its ordinary Docker services. Stopped apps retain their routes and configured replica counts. The stopped state survives deploys, rollbacks, and server restarts; requests do not wake them. Starting a group preserves apps that were individually stopped. Start the group before starting one of its apps. Explicitly stopped dependencies stay stopped even when a consumer starts or is redeployed. See [Compose start and stop](/docker-compose/#start-and-stop-apps).

For native apps, stop also stops workflow workers. Unlike `scale 0`, it prevents automatic cold starts. Start resumes the configured instance count and workflows; for an app configured with zero minimum instances, it warms one instance without changing that minimum.

```bash
tako delete [--env <env>] [--server <server>] [-y|--yes]
tako rm ...
tako remove ...
tako undeploy ...
tako destroy ...
```

Deletes one deployment target, drains and stops the app, removes runtime state and its database bindings, and removes the server-side app data tree for that app/environment on the selected server. Hosted databases are not deleted; use `tako sqlite delete`.

A Compose service cannot be deleted independently: `tako delete --service <service>` is rejected before contacting a server. Remove the service from the Compose file and redeploy, or delete the whole group. Docker named volumes remain after group deletion.

## Releases

```bash
tako release list [--env <env>]
tako release ls [--env <env>]
tako release rollback <release-id> [--env <env>] [-y|--yes]
```

Release descriptions default to the commit subject for clean checkouts, `Uncommitted changes based on: <commit subject>` for dirty checkouts, or `Local source snapshot` without Git history. Set `tako deploy --description "Fix reconnect handling"` to override the description. The dirty status remains visible with a custom description. `release list --json` includes `description`, `commit_message`, `git_dirty`, and `sdk` for each release. `sdk` is `{ "language", "name", "version" }` for the Tako SDK detected at deploy time, or `null` when the release has none.

Lists release/build history or rolls an environment back to a previous release. Rollback uses the standard rolling-update flow and reports partial server failures individually.

## Networks

```bash
tako network create <name> [--alias <alias>]
tako network client add <name> [--network <alias-or-id>] [--deploy <app/env>]... [--expires-in <seconds>] [--identity] [--yes]
tako network client list [--network <alias-or-id>] [--all]
tako network client revoke <name> [--network <alias-or-id>] [--include-invited] [--yes]
tako network join [--alias <alias>] [--invitation-stdin]
tako network sync [network]
tako network export [network] --output <file>
tako network list [--json]
tako network rename <alias-or-id> <new-alias>
tako network status [network] --json
tako network server revoke <server> [--network <alias-or-id>]
tako network server drain <server> [--network <alias-or-id>] [--timeout <seconds>]
tako network server resume <server> [--network <alias-or-id>]
tako connect [network]
tako disconnect [network]
tako ssh <server> [--network <alias-or-id>] [-- <command>...]
```

A positional `[network]` takes an alias or stable ID, like `--network`; passing both with different networks is an error. Without either, these commands use this machine's only network, ask in an interactive terminal when it has several, and fail otherwise. Project commands choose the network of the selected environment instead; see [Choosing a network](#choosing-a-network).

Each machine names its networks by local aliases, unique on that machine; the shared network name is immutable and may repeat across networks. `create` uses the name as the alias; when that alias is taken here, an interactive terminal asks for another, suggesting `<name>-2`, and without a terminal `create` fails and asks for `--alias`. `join` asks for the alias in a terminal, suggesting the network name or `<name>-2` when taken; `--alias` skips the question, and without a terminal `join` uses the name when free and otherwise fails and asks for `--alias`. Rejoining a network this machine already has keeps its alias. `rename` changes only this machine's alias. `list` prints `alias`, or `alias (name)` when they differ; `list --json` returns `{"networks": [{"alias", "name", "id", "device"}]}`. Aliases start with a lowercase letter, use lowercase letters, digits, and `-`, are at most 63 characters, and can't look like a network ID.

Invitations are single-use, expire after 3600 seconds by default (`client add --expires-in` accepts 60–86400), and are redeemed through their issuing server. `join` prompts for the invitation or a client identity, or reads either from stdin. `network sync` pulls published ciphertext without publishing worktree drafts.

Each device generates and retains its own keys. Without `--deploy`, a client gets full access, including the `tako` SSH shell and `connect`; repeat `--deploy` to restrict it to deploying exact app/environment scopes. Deploy clients cannot `connect` or `ssh`. `client add --identity` prints a client identity instead of an invitation: the new client's keys and network profile in one string, on stdout, for machines that keep no state, such as CI. Set it as `TAKO_CLIENT_IDENTITY` and every command acts as that client, keeping its keys in a private temporary directory rather than the Tako home. The identity holds exactly one network, so commands use it without `--network`; a network configured for the environment in `tako.toml` must be that one; `join` with an identity stores it like a joined device. Anyone holding an identity is that client until `client revoke`, so prefer `--deploy` for identities; a full-access identity prints a warning. `client revoke` revokes only that client and lists the clients it invited, which keep their access; `--include-invited` also revokes them and every client they invited, after a confirmation that names them all (`--yes` skips it). It refuses when that set includes the client running the command or would leave the network without a full client, before revoking any of them. Export contains public network configuration, not private keys or app secrets.

`drain` and `resume` require full access and a unique member name or device ID. Drain defaults to 120 seconds and accepts `0..3600`; zero requests immediate force. Inspect the structured completion report, not only the drained intent. See [Native server maintenance](/operations/#native-server-maintenance).

`ssh` uses embedded WireGuard and userspace TCP without sudo or system network changes; OpenSSH is required. When the WireGuard handshake gets no reply, it fails after 10 seconds and names the UDP port and host. `connect` enables local applications, private DNS, short server names, and approved SSH aliases such as `ssh edge-a` for scp, rsync, loopback port forwarding, and VS Code Remote-SSH. It explains the required local administrator access before prompting. `connect` also reports servers without a WireGuard handshake and disconnects any other connected network. `disconnect` removes those managed settings; without a network it disconnects whichever is connected. No external WireGuard executable is required. Normal native management uses pinned HTTPS without a tunnel or SSH agent. See [Native networks](/networks/) for platform requirements, enrollment, endpoints, and private service names.

## Host recovery

Run on a surviving Linux server through its provider console or root SSH:

```bash
sudo tako-server client invite [--expires-in <seconds>] [--json]
```

Requires root and a running native network agent with intact state and keys. Prints a single-use invitation for an independent full client, with a default validity of 3600 seconds (60–86400 accepted). Join from the replacement machine with `tako network join`. See [Recover access](/networks/#recover-access).

## Servers

```bash
tako network server add [admin@host] [--network <alias-or-id>] [--name <name>] [--wireguard-endpoint <ip:udp-port>] [--port <ssh-port>] [--ssh-key <path>] [--http-port <port>] [--https-port <port>]
tako network server list
tako network server ls
tako status
tako network server reload <name> [--network <alias-or-id>] [--force]
tako network server upgrade [name] [--network <alias-or-id>] [--force]
tako network server revoke [name] [--network <alias-or-id>] [-y|--yes]
tako network server remove [name] [--network <alias-or-id>] [--uninstall] [-y|--yes]
tako network server rm [name] [--network <alias-or-id>] [--uninstall] [-y|--yes]
```

`network server add` bootstraps over administrator SSH and enrolls the server's own keys, HTTPS pins, and endpoint in a Tako network. No `[[servers]]` entry is written. Without `--network`, it uses this machine's only network, or asks in an interactive terminal when there are several. Create or join a network first; without a terminal and with several networks, pass `--network`.

Without a host, an interactive terminal prompts for `admin@host`. An omitted SSH user defaults to `root`. Public HTTP and HTTPS ports default to `80` and `443`. `--ssh-key` selects a private key for bootstrap and saves its absolute path locally for SSH maintenance (reload, upgrade, remove). This selection stays on the workstation; normal HTTPS management uses the enrolled device identity. External VPN providers and the old `--vpn`, `--install`, `--no-test`, and server `--description` options are removed. See [Servers](/servers/#choose-the-vpn-provider) to replace an old setup.

`status` prints a deployment snapshot grouped by server, with compact server summary, resources, routes, and app rows. The summary shows how long the host and the Tako server have been up. `Resources` shows the CPU count and load average, memory use (and swap, when the host has any), and free space on the filesystem holding the server's data directory. Memory and disk use turn yellow at 80% and red at 90%; the 1-minute load turns yellow once it reaches the CPU count. `--json` reports the raw figures under `resources` on each server (`host` stays the server's address), `null` when the server reports none: `{"uptime_secs", "cpu": {"cores", "load_average": [<1m>, <5m>, <15m>]}, "memory": {"total_bytes", "available_bytes", "swap_total_bytes", "swap_free_bytes"}, "disk": {"path", "total_bytes", "used_bytes", "available_bytes"}}`. Disk sizes follow `df`: `used_bytes` excludes the blocks reserved for root. A part the server can't read is left out. With several networks, an interactive terminal (without `--json`) asks for all networks or one; `--network` narrows it to one, and without a terminal it shows every network. An app whose instances the server had to replace shows a `Restarts` row with the restart count, the last exit (crashed, unhealthy, or failed to start, with its exit code or signal), and when the next restart is due while restarts are backing off. `--json` reports the same under `recovery` in each app status.

Inside a project (the current directory's `tako.toml`, or `-c`), `status` also shows its resources without reading or showing any value. Under each server, `Databases` lists the Tako-hosted databases homed there and which of the project's app environments bind them. After the servers, an `App` block lists each environment: its configured network and target servers, the number of app secrets (`tako secret list` shows their names), which provider credentials are set, its storage bindings and whether S3 credentials exist, whether a backup key exists, and each database binding with its server. A deploy client sees the databases and servers and a note that secrets need a full client. `--json` adds `databases` (`name`, `bound_by`) to each server and a top-level `project` object, `null` outside a project: `{"app", "environments": [{"name", "network", "access": "local"|"full"|"deploy"|"unknown", "servers", "secrets": <count>|null, "credentials": [<name>]|null, "storages": [{"binding", "resource", "credentials": true|false|null}], "backup_key": true|false|null, "databases": [{"binding", "database", "server"}], "note"}]}`. `null` means this device can't see that value.

Maintenance commands accept the exact selector shown by `network server list --json`, `<alias>/<server>`, which names its own network by this machine's alias. Two servers with the same name in one network get `<alias>/<server>/<device-id>`. A bare server name resolves in `--network`, or in the only network; with several networks, pass `--network`. `upgrade` without a name upgrades every server in that network. Without a name, `revoke` and `remove` prompt with that network's servers, or with every `<alias>/<server>` when the machine has several networks and no `--network`; see [Server maintenance](/servers/#server-maintenance). `network server reload` performs a zero-downtime service reload by default. Its `--force` flag performs a full restart. `network server upgrade` checks the candidate protocol against the CLI and every active app before reload, checks again after readiness, and restores and restarts the previous binary if the upgrade fails. `network server upgrade --force` attempts an incompatible upgrade but keeps readiness and rollback checks enabled. `network server revoke` revokes the server's network membership without deleting app data; clients it approved keep theirs. `network server remove` (alias `rm`) takes the server out of the network for good over administrator SSH: it lists the apps on the server, publishes the revocation, then deletes every app, its releases, volumes, databases, and backups on the server, and the server's network identity and state. Tako stays installed, so `network server add` can enroll the machine in any network again; `--uninstall` also removes the service, binaries, and service files. Revocation failure stops before deletion; cleanup failure after revocation reports removal as incomplete and names the helper to finish it over SSH. Offline peers enforce the revocation when they reconnect. Removing the last server of a network requires `--yes`, as do non-interactive runs. `--dry-run` performs neither revocation nor deletion. `--json` reports `{"network", "server", "apps", "uninstalled"}`; `apps` is `null` when the server could not list them.

## Secrets

```bash
tako secret set <NAME> [--env <env>] [--expires-on <when>]
tako secret add <NAME> ...
tako secret rm <NAME> [--env <env>] [--yes]
tako secret list
tako secret sync [--env <env>] [--network <alias-or-id>] [--retry-expired]
tako secret clear --env <env> [--yes]
tako secret migrate
tako secret key export [--env <env>]
tako secret key import [--env <env>] [--passphrase]
```

Secrets are app-facing values delivered to SDKs. Values are encrypted in ignored worktree-local `.tako/state.local.json`; keys are stored outside the project. Interactive prompts accept pasted multiline values and ask before trimming surrounding whitespace; non-interactive commands read the full stdin stream. Set/remove commands only edit local drafts. An environment without a network keeps them as drafts until its first deploy or `secret sync` picks a network, records it, and publishes them; `--network` on set or remove binds the draft to that network instead, recording it when the environment has none. Deploy and `secret sync` pull published state, preserve local edits, and publish per-key changes for the selected environment. `secret sync` without `--env` syncs every environment that has a recorded network. Sync does not restart running apps; deploy to apply values. `secret list` reads local state only. For each environment it shows `[set] by <device>` with the device that published the current value, `[draft]` for a change in this worktree that is not published yet, or `[set]` when the environment has no network; `--json` adds the same per environment as `last_changed`. Network-bound secrets, provider credentials, storage credentials, and backup keys are replicated as ciphertext between servers. Authorized clients cache them through HTTPS.

`secret clear` deletes every app secret of one environment from that environment's network (or the `--network` one, for that run) and publishes the deletions immediately, unlike `set` and `rm`. It first pulls published state, so values published from other devices or worktrees are deleted too, and it discards this worktree's unpublished app drafts for the environment. Another worktree's unpublished edit to a cleared name is published again by that worktree's next sync or deploy. If clearing stops partway, its deletions remain saved drafts; rerun it, or the next sync or deploy publishes them. Other unpublished drafts, such as provider credentials, stay local. Storage credentials and backup keys are kept. Interactive runs list the names and ask for confirmation; non-interactive runs require `--yes`. `--dry-run` previews from local drafts and the device cache without contacting the network. Running apps keep their values until their next deployment.

Secret state retains the current value per key and versioned deletions. Pending publication requests expire after seven days; `--retry-expired` explicitly replaces expired requests with the current local drafts as new changes. Review the drafts first, since they can overwrite remote changes to those keys.

`secret migrate` validates the original environment keys, writes local state, and removes the old `.tako/secrets.json` after retaining an ignored recovery copy. Commit the tracked file's deletion. Historical commits remain unchanged. See [Secrets](/secrets/) for conflicts, worktrees, and recovery.

`secret key import` can recover the original environment key before local drafts are merged with the cached network state. It accepts an exported key through a masked prompt or stdin; use `--passphrase --env <env>` to derive the original key from its passphrase instead. It rejects keys that cannot decrypt existing values. An invitation restores access to published values, but does not recover unpublished drafts or legacy environment keys.

Expiry accepts `YYYY-MM-DD` or `in N days`. Leave it blank for no expiry. Deploy fails on expired selected-environment secrets and warns when they expire within 30 days.

## Provider Credentials

```bash
tako credential set [name] [--env <env>] [--expires-on <when>]
tako creds set ...
tako credential rm <name> [--env <env>]
tako credential list
```

Running `tako credential` without a subcommand lists credentials. Set and remove prompt for an environment when `--env` is omitted; non-interactive use requires `--env`.

Provider credentials are Tako-owned runtime credentials, not app secrets. Supported names are `ssl.cloudflare` and `postgres_url`. They are encrypted in the local state and the network state, never exposed to app code, and published by `tako secret sync` or deploy. Running processes receive the new bindings on deploy.

`ssl.cloudflare` powers Let's Encrypt DNS-01 and Cloudflare Origin CA. `postgres_url` selects shared Postgres for workflows and, unless `[envs.<env>.channels].database` selects hosted SQLite, for channels, presence and Store invalidations. Selecting a replay database does not change workflow storage or migrate existing history. Multi-server workflows need Postgres unless every workflow opts into local execution. Neither provider credential is exposed to app code.

## Storage

```bash
tako storage add <binding> [--env <env>] [--resource <resource>] [--provider s3|local] [--bucket <bucket>] [--endpoint <https-url>] [--region <region>] [--access-key-id <value>] [--secret-access-key <value>] [--expires-on <when>] [--force-path-style] [--public-base-url <https-url>]
tako storage credentials <resource> [--env <env>] [--access-key-id <value>] [--secret-access-key <value>] [--expires-on <when>]
```

Storage commands default to `--env production` and `--provider s3`.

`storage add` writes the app binding to `tako.toml`. For S3 resources it also writes resource metadata and encrypted credentials. `storage credentials` rotates credentials for an existing top-level S3 resource without adding an app binding, which is useful for backup-only resources.

## Backups

```bash
tako backup now [--env <env>] [--server <server>]
tako backup list [--env <env>] [--server <server>]
tako backup ls [--env <env>] [--server <server>]
tako backup status [--env <env>]
tako backup download <backup-id> [--env <env>] [--server <server>] [--output <path>]
tako backup restore <backup-id> [--env <env>] [--server <server>] [-y|--yes]
```

Backup commands default to `production`, require project context, and target the deployment id `{app}/{env}` over signed HTTP management. Multi-server download and restore require `--server`.

`backup now` creates a manual full app-directory archive; deploys and timers do not create one. `backup list` lists these archives and older `databases` archives. `backup status` shows the last archive, the last failed attempt, retention, and unfinished restores.

`backup restore <backup-id>` restores a manual archive, including workflow state when present, and pauses the owning app. Use `--server` when multiple servers are mapped. `download` saves an encrypted archive. Hosted databases have their own continuous backups; use `tako sqlite backups` and `tako sqlite restore`. See [Backups](/backups/) for retention, restore locks, and recovery on a replacement server.

## Databases

```bash
tako sqlite create <name> [--server <server>]
tako sqlite list
tako sqlite ls
tako sqlite status [<name>]
tako sqlite shell <name> [<sql>]
tako sqlite migration new <name> [--binding <binding>]
tako sqlite migration list [<database>] [--local]
tako sqlite migration apply [<database>] [--local]
tako sqlite backup <name>
tako sqlite backups <name>
tako sqlite restore <name> (--latest|--snapshot <id>|--at <time>) [--server <server>] [-y|--yes]
tako sqlite import <name> <file> [-y|--yes]
tako sqlite export <name> <file>
tako sqlite reset <name> [--local] [--force] [-y|--yes]
tako sqlite delete <name> [--server <server>] [-y|--yes]
tako sqlite rm ...
```

`tako db` is an alias for `tako sqlite`. A database belongs to a network: `--network`, else the network configured in the selected TOML file's environments, else this machine's only network, else an interactive question. Pass `--network <alias-or-id>` when the environments configure several networks. A database's name identifies it within the network; `create` refuses a name the network already has. `list` and `migration` need permission to deploy some app; the others require network and server administrator permission.

`create` makes the database on `--server` (optional on a one-server network) and records its home. Bind it to app code with `[envs.<env>].databases` or select it for runtime replay with `[envs.<env>.channels].database`. `list --json` shows its home and the project's `channel_replay_envs`; `channel_owner` identifies an automatically owned replay database. `status` asks the home for size, users, and backup state, for every database the project uses if no name is given. In its JSON `bindings` and `remote_grants`, `<app>/<env>#channels` denotes runtime replay access. Backups require an environment `backup.storage` or `[databases.<name>].backup`.

`migration new` creates `migrations/[<binding>/]<YYYYMMDDHHMMSS>_<slug>.sql` from the current UTC time (or the next number no migration in the folder uses) and the name, lowercased with other characters replaced by `_`; `--binding` is required when the project binds several names. `migration list` reports the [migrations](/databases/#migrations) each bound database has yet to apply, and `migration apply` applies them, as deploys do; both take an optional database name, and `--local` acts on the development files. `reset` empties a database after a safety snapshot and re-applies the project's migrations; it refuses a database the `production` environment binds without `--force`, and `--local` resets the development file.

`shell` runs one statement when `<sql>` is given and otherwise reads statements from stdin, with `.tables`, `.schema [table]`, and `.quit` in an interactive terminal. Results stop at 1,000 rows and 8 MiB. Each statement commits on its own, so `BEGIN` is rejected. Hosted databases are backed up continuously. `backup` captures a full snapshot now; `backups` lists the window of time the database can be restored to and its restore points with their reason (`full snapshot`, `manual full snapshot`, `before release`, `before migrations`, `before restore`). `restore` replaces the database with its backed-up state: `--at <time>` the newest capture at or before a local time (`2026-11-23 10:15`), an RFC 3339 time, or an age (`15m`, `2h`, `1d`); `--snapshot <id>` a restore point; `--latest` the newest state uploaded. It first backs up the contents it replaces as a `before restore` point. With `--server` it recovers the database onto another server, which becomes its server, and removes the new copy again if the restore fails (keeping the backups); a move that stopped after its restore is finished by running the same command again; the old copy stops uploading backups: at once if its server is up, otherwise once that server is back and sees the move. `import` replaces the database with a local SQLite file; `export` writes a consistent copy and refuses to overwrite an existing file. `delete` removes the database file, its backups, its network records, its `tako.toml` entries, and copies an unfinished move left on other servers of the network. It refuses while apps on the database's server bind it, and revokes grants to app environments on other servers after naming them in a warning. `delete --server <server>` removes only the copy `restore --server` left on an old server, keeping the database, its record, and its backups. `restore`, `import`, `reset`, and `delete` ask for confirmation; non-interactive runs need `--yes`.

Global `--json` prints `{"ok":true,"command":"sqlite",...}`: `databases` for `list` and `status`, `windows` and `snapshots` for `backups`, `migrations` for `migration list` and `apply`, `path` for `migration new`, and the created `database` for `create`. `shell --json` prints one query result per statement. See [Databases](/databases/).

## Workflows

```bash
tako workflow list [--env <env>] [--server <server>] [--status <status>] [--name <workflow>] [--limit <n>]
tako workflow ls ...
tako workflow show <run-id> [--env <env>] [--server <server>]
tako workflow retry <run-id> [--env <env>] [--server <server>]
tako workflow cancel <run-id> [--env <env>] [--server <server>] [-y|--yes]
```

Workflow commands inspect and repair the durable runs of a deployed environment. They default to `production`, require project context, and target the deployment id `{app}/{env}` on every mapped server, or only on `--server`. They are not available for `development`; use the `tako dev` logs locally.

`list` shows the newest runs first: ID, workflow, status, attempts used of the retry budget, the next due time of a pending run or the finish time of a finished one, and the first line of the last error. `--status` accepts `pending`, `running`, `succeeded`, `cancelled`, or `dead`. `--name` selects one workflow. `--limit` accepts `1..200` and defaults to 50. `show` prints every run field, the payload and result, up to the 100 most recent saved steps oldest first, event waits, and the stored error. Servers cap large values; a capped value shows the start of its JSON text and its stored size.

`retry` accepts a `dead` or `cancelled` run. The run becomes `pending`, due now, with a fresh retry budget; saved steps are kept, so it resumes after its last checkpoint. `cancel` accepts a `pending` or `running` run and asks for confirmation in an interactive terminal unless `--yes` is passed. A running handler is not interrupted: it stops at its next checkpoint (the next step, sleep, or wait) and cannot complete the run; side effects already started are not undone. Retry does not prompt. Both act on exactly one server.

Without `postgres_url`, each server keeps its own runs, so results are labelled with their server; if two servers hold a run with the same ID, `show`, `retry`, and `cancel` fail and ask for `--server`. With `postgres_url`, every server shares one store: runs are listed once, without a server, and retry or cancel is sent once. A server that cannot be reached is reported as a warning while the others answer; the command fails only when no server answers, or when the run is not found on any server.

Global `--json` prints one object. Every object has `ok`, `command: "workflow"`, `app`, `environment`, `store` (`"sqlite"` or `"postgres"`), and `errors`, an array of `{name, error}` for servers that did not answer.

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

`completions` installs bash, zsh, and fish completion scripts for shells that are on `PATH`. zsh is installed only when a writable directory is already on `fpath`; otherwise Tako prints how to add one.

When `tako upgrade` installs a new release, it asks the local dev server to shut down across all environments. If the daemon acknowledges shutdown but needs more than a minute to finish, the upgrade succeeds and shutdown continues in the background. Let it finish before running `tako dev` again in each project (with the original `--config` and `--env`, if used). If the CLI is already current, running apps keep serving.

For Compose apps, the daemon logs Docker cleanup errors in `dev-server.log` in the Tako data directory. Containers can remain running after such an error; use `docker ps` to check.

On macOS, official CLI upgrades support Apple Silicon and Intel. Intel builds publish separately after each release. If an Intel download is temporarily unavailable, retry once that build finishes.

`uninstall` removes local Tako binaries, local data, completion scripts, and platform-specific dev services/config after confirmation.

## Usage stats

Official CLI installs send one anonymous event per command so we can count unique users and see which commands run (`deploy`, `dev`, `init`, and so on). The event includes CLI version, OS, architecture, and the command name. It does not include app names, paths, server hosts, environment names, or command arguments. `tako-server` does not send usage stats.

Opt out:

```bash
export TAKO_TELEMETRY=0
```

Usage stats are always off during dry runs and in CI (`CI=true` or `--ci`), even with `TAKO_TELEMETRY=1`. Local Cargo builds are off by default; set `TAKO_TELEMETRY=1` to send from local builds outside CI.
