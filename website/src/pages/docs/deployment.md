---
layout: ../../layouts/DocsLayout.astro
title: "Self-hosted app deployment: server setup, rolling deploys, and scaling - Tako Docs"
heading: Deployment
current: deployment
description: "Guide to deploying apps with Tako on your own servers, including server setup, rolling deploys, scaling, secrets, and production operations."
---

# Deployment

Tako deploys prebuilt local artifacts to your own servers. The CLI builds and uploads; `tako-server` extracts, installs production dependencies, starts app processes, manages TLS, and rolls instances forward without stopping old traffic first.

## Server Setup

Install `tako-server` on each host:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

Then register the server locally:

```bash
tako servers add host.example.com --name la
```

If the server is not installed yet, `servers add` can install or repair it:

```bash
tako servers add ubuntu@host.example.com --install --name la
```

The global server inventory is stored in the platform config directory, not in the project. Each server entry includes host, port, optional description, and detected target metadata (`arch`, `libc`).

## Configure Environments

Map project environments to server names in `tako.toml`:

```toml
[envs.production]
route = "dashboard.example.com"
servers = ["la", "nyc"]
idle_timeout = 300

[envs.staging]
route = "staging.example.com"
servers = ["staging"]
```

The deployment identity on each server is `{app}/{env}`. One physical server can host multiple environments for the same app because each environment gets a separate identity and filesystem path.

`development` is reserved for `tako dev` and cannot be deployed.

## Deploy

```bash
tako deploy
tako deploy --env staging
tako deploy --env production --yes
```

`--env` defaults to `production`. Interactive production deploys ask for confirmation only when the environment is implicit. Passing `--env production` or `--yes` makes the target explicit and skips the prompt.

Deploy requires:

- a declared target environment
- `route` or `routes`
- valid server names
- target metadata for every server
- available secret keys for the target environment

If production has no server mapping and exactly one global server exists, interactive deploy can write that server into `[envs.production].servers`.

## Build And Artifact

The deploy source root is the git root when available, otherwise the selected config file's parent directory. The selected config file's parent directory becomes the app subdirectory inside that source root.

Tako copies the source tree into `.tako/build`, respects `.gitignore`, links `node_modules` from the original tree, runs build stages, merges assets into `public/`, verifies `main`, writes `app.json`, and archives the result. For JavaScript apps, `app.json` also carries `TAKO_APP_ROOT` so deployed app and worker processes discover channels and workflows from the configured `app_root`.

Always excluded:

- `.git/`
- `.tako/`
- `.env*`
- `node_modules/`

Deploy artifacts are cached under `.tako/artifacts/`. Cache keys include source hash, target label, preset source/commit, build commands, include/exclude patterns, asset roots, and app subdirectory. Cached artifacts are checksum/size verified before reuse.

Version naming:

- clean git tree: `{commit}`
- dirty git tree: `{commit}_{source_hash8}`
- no git commit: `nogit_{source_hash8}`

## Server Preparation

Servers receive prebuilt artifacts. They do not run app build steps.

After extraction, `tako-server`:

- reads `app.json`
- downloads or resolves the pinned runtime when needed
- runs the runtime plugin's production install command
- prepares app data directories
- stores secrets when they changed
- starts workflow supervision for the release

Production install receives the release env plus minimal process env (`PATH`, `HOME` when available). It does not inherit arbitrary `tako-server` service env vars.

## Release Commands

Configure a release command when you need migrations or cache preparation before the rolling update:

```toml
release = "bun run db:migrate"
```

Override or clear per environment:

```toml
[envs.staging]
release = ""
```

The release command runs once on the leader server, after extract and production install, before rolling update. The leader is the first server listed for the environment. Followers wait for the leader result.

The command runs as `sh -c` in the new release directory. It receives app env, deploy secrets, `TAKO_BUILD`, `TAKO_DATA_DIR`, and `PATH` if not already present in the app/release env. It starts from a cleared service environment and has a 10-minute timeout.

If the command fails or times out, deploy aborts on every server. Partial release directories are cleaned up and old instances keep serving.

## Rolling Updates

For each server, Tako replaces instances with this pattern:

1. Start one new instance.
2. Wait for fd-4 readiness and health checks.
3. Add the new instance to the load balancer.
4. Drain one old instance.
5. Stop the drained instance.
6. Repeat until the release is active.
7. Update the `current` symlink.
8. Clean up old releases older than 30 days.

Rollback on failure keeps old instances serving. If some servers succeed and others fail, the CLI reports partial failures at the end.

## Scaling

```bash
tako scale 2 --env production
tako scale 0 --env production
tako scale 3 --env production --server la
```

Desired instance count is stored on each server as runtime state. It persists across deploys, rollbacks, and restarts.

New apps start with desired instances `1`. Scaling to `0` enables scale-to-zero. Deploy still starts one warm instance for the new release, then idle shutdown can stop it after `idle_timeout`.

Cold starts wait for readiness up to the startup timeout. If no healthy instance is ready, the proxy returns `504`. If startup setup fails, it returns `502`. If too many requests are waiting for a cold start, it returns `503`.

## Secrets

```bash
tako secrets set DATABASE_URL --env production
tako secrets sync --env production
```

Local secrets are encrypted in `.tako/secrets.json`. Deploy decrypts the target environment's secrets locally, compares the remote secrets hash, and only sends secrets when the server needs them.

On the server, secrets are encrypted in SQLite. Fresh app and worker processes receive them through fd 3, not through release files.

Secret changes can be synced after deploy:

```bash
tako secrets sync --env production
```

`tako-server` stores the new secrets, drains/restarts workflow workers, and rolls HTTP instances so fresh processes receive updated values.

## TLS And Wildcards

Tako issues certificates automatically. Exact hosts use HTTP-01. Wildcard hostnames require DNS-01 through lego.

Configure DNS-01 credentials:

```bash
tako servers setup-wildcard
```

The command currently applies DNS configuration to all configured servers. It accepts `--env`, but that flag does not filter targets.

Credentials are stored on the server at:

```text
/opt/tako/dns-credentials.env
```

The provider name is persisted in:

```text
/opt/tako/config.json
```

## Remote Management

`tako-server` exposes management RPC over HTTP on port `9844` for Tailscale-reachable operations. `hello` and `server_info` are public probes. Other RPCs require SSH-key-signed headers, a fresh timestamp, and a non-replayed nonce.

SSH remains part of deployment for upload, setup, recovery, and log access.

## Logs And Status

```bash
tako servers status
tako logs --env production
tako logs --env production --tail
tako logs --env production --json
```

`servers status` queries all configured servers and shows service state plus deployed app/build status. `logs` fetches app stdout/stderr and app-scoped server diagnostics from environment servers.

## Release History And Rollback

```bash
tako releases ls --env production
tako releases rollback abc1234 --env production --yes
```

Release history is grouped by release id and sorted newest-first. Rollback reuses the same rolling-update path as deploy and keeps current routes, env, secrets, and scale settings.

## Delete A Deployment

```bash
tako delete --env production --server la
```

Delete removes one app/environment/server target. It stops and drains processes, removes app state/routes, and deletes `/opt/tako/apps/{app}/{env}`.

Interactive mode can discover the target. Non-interactive mode requires `--yes`, `--env`, and `--server`.

## Upgrade Servers

```bash
tako servers upgrade
tako servers upgrade la
```

Server upgrade verifies signed checksums, installs the new binary, enters upgrade mode, reloads the service, waits for the new primary process, and exits upgrade mode. If readiness fails after binary replacement, the previous binary is restored.
