---
layout: ../../layouts/DocsLayout.astro
title: "Troubleshooting - Tako Docs"
heading: Troubleshooting
current: troubleshooting
description: "Troubleshoot common Tako problems including deploy failures, TLS issues, runtime errors, server status problems, and verbose diagnostics."
---

# Troubleshooting

Start with local diagnostics:

```bash
tako doctor
```

For remote state:

```bash
tako status
tako logs --env production
tako logs --env production --tail
```

Use `--verbose` for a timestamped transcript, `--ci` for deterministic non-interactive output, and global `--json` when an agent needs structured stdout.

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
- Missing `postgres_url` for multi-server channel/workflow deployments.
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

Container releases require Podman on the target server. `tako servers add --install` installs it, and server upgrade installs it when missing.

The container must:

- Listen on `HOST=0.0.0.0` and `PORT=3000`.
- Use a Tako SDK so `/status` handles the internal health probe.
- Read secrets and storage bindings through SDK bootstrap, not individual env vars.

In v0, container HTTP instances do not receive fd 3, fd 4, the internal socket, or `TAKO_DATA_DIR`.

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

Normal app operations use signed HTTP management on the server's private Tailscale address. SSH is used for setup, repair, reload, upgrade, and uninstall flows.

If a server is unreachable:

```bash
tako status
tako servers reload <name>
```

Use `tako servers add --install <host>` or `tako servers add admin@host` to install or repair a server before adding it to `config.toml`.
