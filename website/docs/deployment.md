---
title: Deployment
description: "Build and deploy a Tako release: select an environment, package native or container artifacts, run release commands, and verify rolling updates."
---

Tako deploys locally built apps to servers you control. The CLI validates project state, builds and packages the release, uploads artifacts over signed private HTTP management, and asks `tako-server` to prepare and roll the release into traffic.

## Server Setup

Provision a supported Linux host and enroll private management access using [Servers](/servers/#server-setup). That page covers system requirements, isolation, installation, and recovery access.

## Add A Server

Use [Servers](/servers/#add-a-server) to add or repair a host. Confirm the inventory name with `tako servers list --json`, then map it to the environment below.

## Configure An Environment

```toml
[envs.production]
route = "app.example.com"
servers = ["la"]
```

Deploy targets the servers listed in `[envs.<env>].servers`. If production has no server mapping and exactly one global server exists, deploy can select it and persist the mapping. Other non-development environments require explicit server mappings.

## Deploy

```bash
tako deploy --env production
tako deploy --env staging
```

When `--env` is omitted, deploy targets `production`. In an interactive terminal, an implicit production deploy asks for confirmation — but only if `tako.toml` declares more than one deployable environment (`development` doesn't count). With a single environment, there's nothing the deploy could have implicitly missed, so it proceeds without asking. Passing `--env production`, `--yes`, or `-y` also skips the prompt by making the target explicit.

The CLI, server, and release artifact must use the same protocol version. Tako checks before parallel deployment starts and again on each server before dependency preparation, release commands, workflow synchronization, or app startup. A mismatch fails the deploy without starting those workloads. `--force` attempts the incompatible deploy but does not skip validation or readiness checks; unlike `--yes`, it does not control confirmation prompts.

Inspect a mismatch before choosing to force it. For automation, select the environment explicitly and use `--yes` only when the deployment is already authorized.

Deploy validates secrets, storage credentials, provider credentials, routes, server target metadata, workflow/channel storage requirements, and local-storage limitations before build work starts.

## Build And Artifact Rules

Build precedence:

1. `[[build_stages]]`
2. `[build]`
3. Runtime default
4. No-op

Tako copies the source root into `.tako/build`, respects `.gitignore`, symlinks `node_modules` from the original tree for JS builds, omits source symlinks, runs build commands, merges assets into `public/`, verifies the resolved `main`, and archives the result without `node_modules`.

The final native artifact includes regular files only; build-created symlinks are omitted too. Do not depend on linked files being deployed. This differs from app data backups, which preserve symlinks.

Always-excluded paths: `.git/`, `.tako/`, `.env*`, and `node_modules/`.

Version names are based on git state: clean commit hash, dirty commit plus content hash, or `nogit_<hash>` when no git commit is available.

## Native Releases

Native releases use runtime plugins or explicit `start` commands. JS runtimes run through SDK entrypoint wrappers. Go binaries run directly. Explicit `start` commands skip runtime defaults and runtime version probing.

Runtime download or version resolution failures abort release preparation. Runtimes without a download definition use the server's PATH. Production dependency installation must succeed before the new release starts; dependency directories are not included in the uploaded archive.

Native HTTP instances bind `127.0.0.1` on an OS-assigned port and signal readiness through fd 4. Secrets, storage bindings, and the internal health token arrive through fd 3.

## Container Releases

The [Docker section](/docker/) covers image configuration, [Dockerfiles by language](/dockerfile/), and [container workflow workers](/docker-workflows/).

```toml
container = "Dockerfile"
```

Container releases upload source and let `tako-server` build the image with Podman. HTTP containers run from the image defaults and receive `HOST=0.0.0.0`, `PORT=3000`, app vars, `TAKO_BUILD`, and `TAKO_BOOTSTRAP_DATA`.

Use a Tako SDK inside the container so secrets, storage bindings, internal status, and health-probe authentication follow the same contract as native releases. In v0, container HTTP instances do not receive fd 3, fd 4, the internal socket, or `TAKO_DATA_DIR`.

A configured workflow `run` starts a separate container process from the same image with an entrypoint override, args from `run[1..]`, a mounted internal socket, and `TAKO_BOOTSTRAP_DATA`. In v0, container releases support one configured workflow `run` command across the base workflow config and named groups.

## Release Commands

```toml
release = "bun run db:migrate"

[envs.staging]
release = ""
```

The release command runs once on the leader server after extract and production install but before rolling update. Followers wait for the leader result. If the command fails, times out, or exits by signal, deploy aborts on every server and leaves the old release serving.

The command runs with the same env as new HTTP instances, plus freshly decrypted secrets as env vars. The hard timeout is 10 minutes.

Release commands, production installs, and container builds retain the last 64 KiB of each output stream for diagnostics. Output is drained throughout execution, including when a command writes binary data.

## Rolling Updates

For each server, Tako starts replacement instances, waits for health, keeps the new batches out of public request routing until each stays healthy for a short stability window, then routes traffic to the stable replacement set and drains the old instances. The `current` symlink moves only after the rolling update succeeds.

During validation, a server may temporarily run both the old instance set and the new replacement set.

After finalize, each server keeps the active release and prunes non-active releases older than 30 days or beyond 50 total releases.

If desired instances are `0`, deploy still starts one warm instance for the new build so traffic works immediately. It can later idle out.

Failures, including a new instance becoming unhealthy during the stability window, keep old instances running, clean partial release directories when needed, roll back release metadata, and report per-server results.

After rollout, inspect `tako status --json`, request the real route, and read finite app logs. Verify the changed behavior through the application; successful upload or rollout alone does not prove that a database migration or user flow works.

## Scaling

See [Operations](/operations/#scaling) for changing desired instance counts, scale-to-zero behavior, and post-change verification. Scaling is server-side state rather than a `tako.toml` instance-count field.

## Secrets And Credentials

Configure [app secrets and provider credentials](/secrets/) before deployment. App secrets feed your SDK; `ssl.cloudflare` and `postgres_url` configure Tako-owned runtime services. Deployment sends the relevant changed values.

## Storage And Backups

Use [Storage](/storage/) to bind app files and [Backups](/backups/) to protect the deployed data tree. Backup-only resources are not exposed as app storage bindings.

## TLS

See [Routing and TLS](/routing/) for domain routes, certificate providers, wildcard credentials, public ports, and client-IP policy.

## Server Maintenance

See [Servers](/servers/#server-maintenance) for reloads, protocol-checked upgrades, recovery, and uninstall behavior.

## Logs And Releases

Use [Operations](/operations/#logs-and-releases) to inspect release history, collect finite logs, and roll back code. Verify the deployed route after every release.

## Metrics

The Prometheus endpoint and metric names are documented in [Operations](/operations/#metrics).
