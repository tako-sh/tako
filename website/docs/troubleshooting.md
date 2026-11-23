---
title: Troubleshooting
description: "Troubleshoot common Tako problems including deploy failures, TLS issues, runtime errors, server status problems, and verbose diagnostics."
---

Start with local diagnostics:

```bash
tako doctor --json
```

For remote state:

```bash
tako status --json
tako logs --env production --days 1 --json
```

Use `--verbose` for a timestamped transcript, `--ci` for deterministic non-interactive output, and global `--json` when an agent needs structured stdout. `tako dev --json` prints JSONL (a ready object, then log and event records) with no logo or TUI.

Choose the failing subsystem before changing it: [Local Development](/development/) for startup, [Servers](/servers/) for management access, [Deployment](/deployment/) for builds/rollouts, [Routing and TLS](/routing/) for domains/certificates, [Secrets](/secrets/) for decryption or missing values, and [Storage](/storage/) or [Backups](/backups/) for file/data problems. Use finite logs for tool calls; keep `--tail` for an intentionally attached terminal.

## `tako dev` Will Not Start

If the daemon cannot start, Tako reports the last lines from the dev-server log. Common causes:

- The HTTPS daemon port `127.0.0.1:47831` is already in use.
- macOS launchd dev proxy setup needs repair or sudo approval.
- Linux redirect rules or systemd-resolved setup are missing.
- The local CA is not installed or trusted yet.

Run:

```bash
tako doctor
tako dev
```

On macOS, doctor checks the dev proxy, boot helper, loopback alias, launchd load state, local DNS, and TCP reachability on the loopback HTTP/HTTPS ports.

## `tako ssh` Cannot Reach a Server

`tako ssh` needs the server's WireGuard UDP endpoint, not public TCP port 22. Run:

```bash
tako doctor
```

The network section reports each server's handshake:

- **no reply**: UDP to the endpoint isn't getting through. Allow inbound UDP on the WireGuard port (default `51820`) in the server firewall and the provider's security group, and check that your current network allows outbound UDP. If those are open, the server may not have applied this device's membership yet; it drops handshakes from unknown devices without answering.
- **refused**: nothing is listening on that UDP port, or a firewall is rejecting it. Check that `tako-server` is running and that the advertised endpoint is the server's current public address.
- **reachable** but `tako ssh` still fails: the server's private SSH listener did not answer. Check `tako-server` on that host.

There is no relay for servers behind NAT or networks that block UDP. Use the provider console or root SSH to recover access.

## `.test` Hostnames Do Not Resolve

Tako manages DNS for `.test` and `.tako.test` only. External development routes are routed by the proxy but must resolve to your machine through your own DNS or tunnel setup.

If `.test` conflicts with an existing resolver on macOS, Tako leaves the existing resolver alone and `.tako.test` remains available as a fallback. On Linux, ensure the systemd-resolved route for `~test` and `~tako.test` points at the local DNS listener.

## Vite Dev Never Becomes Ready

Direct Vite dev commands must use the `tako.sh/vite` plugin so the dev server can signal readiness on fd 4. Tako does not scrape Vite stdout URLs as readiness.

Add the Vite plugin and use a preset or command that runs Vite through the project install:

```toml
runtime = "bun"
preset = "vite"
```

## Deploy Fails Before Building

Deploy performs validation before build work starts. Check for:

- Target environment missing from `[envs.<env>]`.
- Missing `route` or `routes` on a non-development environment.
- Unknown servers in `[envs.<env>].servers`.
- Missing or invalid server `arch`/`libc` metadata. Check that the enrolled host uses a supported Linux architecture and glibc.
- Expired app secrets, S3 credentials, or provider credentials.
- Missing `ssl.cloudflare` for wildcard Let's Encrypt routes or Cloudflare SSL.
- Missing `postgres_url` for shared multi-server workflows. Native channel, presence, and store delivery do not require it; see [workflow storage](/workflows/).
- Local storage used in a multi-server deploy environment.

Use:

```bash
tako credential list
tako secret list
tako network server list
```

## Missing Runtime Entrypoint

Tako resolves `main` from `tako.toml`, then the runtime manifest field such as `package.json` `main`, then the preset. If none resolve, deploy and dev fail with guidance.

For JS frameworks, make sure your preset matches the build output. TanStack Start expects `dist/server/tako-entry.mjs`; Next.js expects `.next/tako-entry.mjs`. Those files are emitted by the corresponding Tako adapters/plugins.

## Container Release Does Not Start

Container releases require Podman on the target server. `tako network server add --network <network>` installs it, and server upgrade installs it when missing.

The container must:

- Listen on `HOST=0.0.0.0` and `PORT=3000`.
- Use a Tako SDK so `/status` handles the internal health probe.
- Read secrets and storage bindings through SDK bootstrap, not individual env vars.

In v0, container HTTP instances do not receive fd 3, fd 4, or the internal socket. They do receive the app data directory, mounted at `container_options.data_dir` (`/tako/data` by default), with `TAKO_DATA_DIR` set to that path. Files written elsewhere in the container are lost when it is replaced.

## App Keeps Restarting

When instances exit or fail health checks, the server replaces them and backs off between repeated attempts, up to 30 seconds. `tako status` shows a `Restarts` row under the app:

```text
web  healthy 0/0  production  abc123
  Restarts  4  last failed to start (exit code 1) Nov 12, 2026 10:00  next restart in 16s
```

- `crashed`: the process exited after it had been healthy. Check the exit code or signal; signal 9 often means the host ran out of memory.
- `unhealthy`: the process kept running but stopped answering health checks.
- `failed to start`: the instance never became healthy, often a missing secret, a startup exception, or a port that was never reported.

Read the app's output with `tako logs --env production --days 1`. `tako status --json` reports the same fields under `recovery`. The server keeps retrying on its own; a fixed deploy or `tako scale` starts new instances at once without waiting for the backoff. The `Restarts` row clears once the app has stayed healthy, or idle, for about a minute.

A deploy or release command that arrives while the server is repairing the app can fail with `Automatic recovery of <app> is in progress; retry in a moment.` Retry the command: recovery pauses for about a minute after turning a command away, so the retry gets through as soon as the current repair step ends. `tako secret sync` publishes network state without restarting the app and does not take this app lock.

## TLS Or Certificate Problems

Let's Encrypt exact routes use HTTP-01 by default, so public port 80 must reach the server's HTTP listener. If you use wildcard routes, or if exact routes should use DNS-01, set:

```bash
tako credential set ssl.cloudflare --env production
```

For `ssl = "cloudflare"`, use a Cloudflare token with Origin CA certificate edit permission. Cloudflare Origin CA certificates are intended for Cloudflare-proxied traffic; direct browser connections to the origin will not trust them.

## Logs Are Empty Or Incomplete

`tako logs` reads app `current.log` and `previous.log` over signed HTTP management. History mode shows the last three days by default:

```bash
tako logs --env production --days 7
```

Use `--tail` for streaming and global `--json` for structured output:

```bash
tako logs --env production --tail
tako logs --env production --json
```

History mode with `--json` returns one object with a `logs` array. `--tail --json` emits one JSONL event per stdout line until interrupted.

Remote fetch failures are command failures, not empty log results.

## Workflow Runs Fail Or Stall

Logs show each attempt's output. To see where a run stopped, read its stored state:

```bash
tako workflow list --env production --status dead
tako workflow show <run-id> --env production
```

`show` lists the saved steps and the stored error. The run failed after its last saved step. Fix the cause, then `tako workflow retry <run-id> --env production` resumes it with a fresh retry budget. A `pending` run with no due time is waiting for a signal; check its waits. A `running` run whose worker was lost is reclaimed 60 seconds after its last heartbeat.

These commands work against deployed environments only. When servers keep separate workflow state, a server that cannot be reached is reported as a warning and its runs are missing from the list. See [Inspect and repair runs](/workflows/#inspect-and-repair-runs).

## Backups Fail

Backups require a private S3-compatible storage resource and current storage credentials:

```toml
[envs.production]
backup = { storage = "private_backups" }
```

Set or rotate credentials with:

```bash
tako storage credentials private_backups --env production
tako backup status --env production
tako backup now --env production
```

Backup storage must not use `public_base_url`, and `local` cannot be used as a backup target.

For a hosted database, check `backup.last_error` and `backup.replicated_at_unix_secs` in `tako sqlite status <name> --json`. Its backups use the storage named in `[databases.<name>].backup`, or else the environment's `backup.storage`; deploy as an administrator after changing it or its credentials so the database's server receives them. Then capture a full snapshot with `tako sqlite backup <name>`. While storage fails, the server keeps the captures on its disk and retries at most 15 minutes apart. A restore needs free space for about three times what it downloads on the server, beyond what other restores and transfers in progress there hold. A database larger than about 5 GiB cannot be backed up to S3 or R2, since a full snapshot is uploaded as one object.

## Database Requests Fail

`tako.databases.<binding>` is `undefined` (or `GetDatabase` reports false) when the environment does not bind that name: check `[envs.<env>].databases` and deploy. A `401` means the token is not valid on this server, usually from a connection copied from another environment or session. A `503` means the database is being restored, imported, or removed; retry. For a database on another server, a `503` saying the database's server has not granted this app access means the binding has no grant key yet: deploy the environment as an administrator. A `403` from the database's server means it holds no grant for the environment, for example after the database was deleted and recreated: deploy as an administrator again. A `502` means this server could not reach the database's server over the private network: check `tako network status --json`. A deploy that fails migrating a database names the migration and SQLite's error; fix it in a new migration file (or restore the edited file, when it says an applied migration changed) and deploy again. A deploy that says the network has no database with a bound name needs `tako sqlite create <name>`, or a binding to one of the names `tako sqlite list` shows. See [Databases](/databases/#limits-and-security).

## Secret Or Credential Key Problems

For values in the network state, check `tako network status --json`, device permissions, and `tako network sync` first. A newly enrolled full client can decrypt the network state; a deploy client cannot decrypt any of it. Unpublished drafts remain on the original worktree.

For a legacy tracked secret file or local encrypted draft with a missing environment key, recover the original key:

```bash
tako secret key import --env production
```

Use `--passphrase --env production` if the original key came from a passphrase. Import validates existing ciphertext before installing the key and does not need to decrypt the shared cache first. Keep the legacy file or local drafts while recovering; generating a replacement key does not decrypt them. Once the original key is available, migrate a tracked file with `tako secret migrate`, then publish with `tako secret sync --env production`.

If Tako reports unpublished secrets for another network or app, this checkout has drafts for the environment's previous network or for another app. Confirm the selected `--config` file and its `[envs.<env>].network`. Publish the drafts with `tako secret sync --env <env> --network <old>`, or remove them; published drafts follow the environment on their own. Files in the same directory share `.tako/` state. See [Move an environment to another network](/secrets/#move-an-environment-to-another-network) and [alternate configurations](/secrets/#use-an-alternate-configuration).

On macOS, iCloud Keychain storage requires the signed `Tako.app` CLI. If Keychain entitlement is unavailable, reinstall Tako with the official installer.

## Server Commands Fail

Normal app operations use signed, pinned HTTPS with an enrolled device identity. SSH is used for setup, repair, reload, upgrade, and remove flows.

If a server is unreachable:

```bash
tako status
```

Use `tako network server add admin@host --network <network>` to install and enroll a server.

Check network membership (`tako network status <network> --json`), HTTPS reachability, device permissions, and SSH recovery access before choosing a repair action. Reload requires a reachable host and is not a diagnostic probe; follow [Servers](/servers/) for maintenance.

If a command says this machine has several networks, pass `--network` with one of the aliases it lists, or run it in a terminal to choose. Inspect `tako network list --json` for aliases and IDs and `tako network server list --json` for full `<alias>/<server>` maintenance selectors. Two networks with the same shared name get different aliases; change one with `tako network rename <alias> <new-alias>`, which affects only this machine. Do not revoke a device to resolve a name collision.

If a command says an environment uses a network this machine hasn't joined, check `[envs.<env>].network` in the selected TOML config. Join it with `tako network join`, or pass `--network` to use another network for that run. The config accepts only a stable 32-character lowercase hexadecimal network ID, not a local alias.
