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
- Missing or invalid server `arch`/`libc` metadata. Re-add the server with SSH checks enabled.
- Expired app secrets, S3 credentials, or provider credentials.
- Missing `ssl.cloudflare` for wildcard Let's Encrypt routes or Cloudflare SSL.
- Missing `postgres_url` for multi-server Channels, Presence, Stores, or global workflows.
- Local storage used in a multi-server deploy environment.

Use:

```bash
tako credentials list
tako secrets list
tako servers list
```

## Missing Runtime Entrypoint

Tako resolves `main` from `tako.toml`, then the runtime manifest field such as `package.json` `main`, then the preset. If none resolve, deploy and dev fail with guidance.

For JS frameworks, make sure your preset matches the build output. TanStack Start expects `dist/server/tako-entry.mjs`; Next.js expects `.next/tako-entry.mjs`. Those files are emitted by the corresponding Tako adapters/plugins.

## Container Release Does Not Start

Container releases require Podman on the target server. `tako servers add --network <network>` installs it, and server upgrade installs it when missing.

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

A deploy, release command, or `tako secrets sync` that arrives while the server is repairing the app can fail with `Automatic recovery of <app> is in progress; retry in a moment.` Retry the command: recovery pauses for about a minute after turning a command away, so the retry gets through as soon as the current repair step ends. A scheduled backup can turn a command away the same way, with `A scheduled backup of <app> is in progress; retry in a moment.`; the retry gets through because scheduled backups pause for about a minute.

## TLS Or Certificate Problems

Let's Encrypt exact routes use HTTP-01 by default, so public port 80 must reach the server's HTTP listener. If you use wildcard routes, or if exact routes should use DNS-01, set:

```bash
tako credentials set ssl.cloudflare --env production
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
tako workflows list --env production --status dead
tako workflows show <run-id> --env production
```

`show` lists the saved steps and the stored error. The run failed after its last saved step. Fix the cause, then `tako workflows retry <run-id> --env production` resumes it with a fresh retry budget. A `pending` run with no due time is waiting for a signal; check its waits. A `running` run whose worker was lost is reclaimed 60 seconds after its last heartbeat.

These commands work against deployed environments only. When servers keep separate workflow state, a server that cannot be reached is reported as a warning and its runs are missing from the list. See [Inspect and repair runs](/workflows/#inspect-and-repair-runs).

## Backups Fail

Backups require a private S3-compatible storage resource and current storage credentials:

```toml
[envs.production]
backup = { storage = "private_backups" }
```

Set or rotate credentials with:

```bash
tako storages credentials private_backups --env production
tako backups status --env production
tako backups now --env production
```

Backup storage must not use `public_base_url`, and `local` cannot be used as a backup target.

## Secret Or Credential Key Problems

Secrets, storage credentials, and provider credentials are encrypted per environment. If another machine cannot decrypt them, import the environment key:

```bash
tako secrets key import --env production
```

On macOS, iCloud Keychain storage requires the signed `Tako.app` CLI. If Keychain entitlement is unavailable, reinstall Tako with the official installer.

## Server Commands Fail

Normal app operations use signed, pinned HTTPS with an enrolled device identity. SSH is used for setup, repair, reload, upgrade, and uninstall flows.

If a server is unreachable:

```bash
tako status
```

Use `tako servers add admin@host --network <network>` to install and enroll a server.

Check network membership (`tako network status <network> --json`), HTTPS reachability, device permissions, and SSH recovery access before choosing a repair action. Reload requires a reachable host and is not a diagnostic probe; follow [Servers](/servers/) for maintenance.
