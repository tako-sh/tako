---
title: Docker Compose
description: "Deploy an existing Compose stack to one VPS with persistent named volumes, Tako HTTPS, service logs, status, and application rollback."
---

Use Compose for an application with multiple services, such as a web server, worker, Postgres, and Redis. Services use their existing images and commands; a Tako SDK is optional.

## Configure a stack

Keep `compose.yaml` and its build contexts in the application directory. Add:

```toml tako.toml
name = "my-stack"

[compose]
file = "compose.yaml"
service = "web"
port = 8080
health_path = "/health"

[envs.production]
route = "app.example.com"
servers = ["prod"]
```

`service` selects the HTTP ingress service and `port` is its container port. `health_path` defaults to `/` and must return HTTP 2xx or 3xx. Set `health_host = "app.example.com"` if the application restricts allowed Host headers. Define dependency health checks in Compose and use `depends_on` conditions where startup ordering matters.

The target server needs Docker Engine and the Docker Compose plugin **2.24.4 or newer**, with local Docker socket access for the `tako-server` service account. Docker is required for Compose deployments; standalone Dockerfile/image releases continue to use Podman. The Tako installer currently installs Podman. Install and configure Docker separately for a Compose host, then verify `docker compose version` and `docker info` as the service account.

Explicit service `platform` selection requires Docker Engine 28 or newer.

```bash
tako secrets set DATABASE_PASSWORD --env production
tako deploy --env production
tako status --json
tako logs --env production --days 1 --json
```

Reference secrets explicitly in Compose, for example `POSTGRES_PASSWORD: ${DATABASE_PASSWORD:?Set DATABASE_PASSWORD}`. Tako supplies app variables and decrypted secrets to Compose interpolation. It does not write interpolated configuration or secrets into release artifacts. Docker's own container metadata contains the environment values that the Compose file assigns. `DOCKER_*`, `COMPOSE_*`, loader variables, `PATH`, and `HOME` are reserved for the client.

Production does not load an implicit `.env` file. Use Tako variables and secrets for interpolation, or an explicit project-local service `env_file` for container environment values. File references must be literal relative paths; interpolation in file paths is unsupported.

Before invoking Docker, Tako copies production inputs into a service-owned private directory and validates the Compose model and every build input. Builds, restarts, secret rotation, and rollback use that captured source and pinned images. Editing an uploaded release after preparation does not change its prepared deployment; deploy a new release to change it. Decrypted interpolation values remain in memory.

Build contexts, Dockerfiles, additional file contexts, build secrets, runtime configs/secrets, and bind sources must stay inside the app directory. Additional `service:` build contexts are supported. Source must contain only regular files and directories, without symlinks or hard links, and fit within 2 GiB, 100,000 entries, and 128 directory levels. The Compose file is limited to 16 MiB. The root `logs` directory is reserved for runtime output and excluded from preparation.

## Networking and persistence

Tako replaces Compose port publications. Only the selected HTTP service is published, on an assigned loopback port; Tako handles public HTTPS and routes. Databases, caches, and workers communicate through Compose networks and service names.

Use named volumes for persistent data. The project identity is stable for one app/environment on one server, so new releases reuse the same volumes. Anonymous volumes are rejected in production. If an image declares a volume, mount a named volume there, or explicitly use tmpfs for disposable data. Read-only bind mounts and config/secret files must stay inside the uploaded app directory. Tako retains older release files while containers reference them.

Networks, volumes, configs, and secrets are scoped to the app/environment. External resources and custom resource names are rejected in production. Networks use Docker's bridge driver and volumes use its local driver; driver options and custom IPAM settings are unsupported.

Named volumes survive deployment, stop/start, rollback, and application deletion. Deleting data requires an explicit Docker volume operation. Tako's app-data backups do **not** include Docker named volumes. Use database-native dumps or a coordinated volume backup, and verify restoration into a separate stack. Application rollback does not reverse schema migrations or restore database contents.

## Updates and recovery

Tako prepares and pins all images before changing running containers. It compares each service's image configuration and filesystem, plus its resolved Compose configuration, referenced secrets, and mounted file contents. Build attestations and release directory names do not trigger replacements. Unchanged services keep their container IDs, network endpoints, and volumes. Updating a Bun web app therefore leaves unchanged Redis running.

| Service | Default update order |
| --- | --- |
| Selected HTTP service without writable persistent volumes | Start replacement, verify readiness, switch traffic, drain old container |
| Selected HTTP service with writable persistent volumes | Stop old container before starting its replacement |
| Workers and other services | Stop old container before starting its replacement |

Override the order for a service using standard Compose settings:

```yaml
services:
  worker:
    image: example/worker:1
    deploy:
      replicas: 2
      update_config:
        order: start-first
        monitor: 5s
```

Use `start-first` only when old and new containers can safely overlap, including access to shared data or job queues. Allocate enough memory for both generations. `monitor` defaults to 5 seconds and supports 0 seconds through 2 minutes. New containers must remain running and pass any Docker health check for this window; unchanged healthy containers skip it. Tako also checks HTTP readiness and a stability window before routing to the selected service. Dependencies follow `service_started`, `service_healthy`, and `service_completed_successfully`; unchanged completed jobs are not rerun.

If readiness fails, Tako removes the new containers and restarts any predecessors it stopped. Healthy unchanged containers remain running. Failed recovery is reported as an application error. The active release, routes, and encrypted credentials commit together. On a server restart, Tako reconciles that committed release, adopts matching containers, and removes abandoned generations. A normal Tako server shutdown leaves production Compose containers running for adoption on startup.

Start-first updates protect traffic through Tako's HTTP ingress. Updates to databases or other dependencies can still cause downtime, and migrations must remain compatible with the versions that overlap. Internal clients using Docker DNS can see a candidate before Tako admits it to public HTTP routing. Existing requests drain for up to 30 seconds; longer requests or persistent connections need application-level reconnect handling.

`tako status --json` includes original service names, state, health, and exit codes. Log prefixes include the service name and generation. The stack has one Tako HTTP instance; define worker replicas in Compose. Production supports one server, one HTTP ingress replica, and up to 128 replicas per other service.

Production rejects writable bind mounts, outside build contexts, `container_name`, `network_mode`, profiles, links, `volumes_from`, shared service namespaces, Compose lifecycle hooks, optional dependencies, and `depends_on.restart`. `update_config` supports `order` and `monitor`; `rollback_config` is unsupported. These restrictions keep replacement and recovery behavior explicit.

Host privileges and resource sharing are unsupported: this includes `privileged`, added capabilities, devices, host namespaces, custom runtimes, Docker socket injection, and providers. `security_opt` supports only `no-new-privileges`. Logging uses `json-file`, `local`, or `none`, with `max-size`, `max-file`, and `compress` options. Builds reject SSH forwarding, entitlements, host networking, and cache import/export settings. Compose `include` and `extends` are unsupported. Unrecognized production service and build settings fail validation before Docker creates containers.

## Verify and develop locally

Request the deployed hostname, write a record or upload through the app, deploy again, and confirm that the data remains. Also test a service failure and restore a backup into a separate environment.

Run `tako dev` from the app directory. Tako builds and starts the whole Compose stack, waits for service and HTTP health, and serves the selected service through the usual local HTTPS hostname. Docker Engine and Compose must be available locally; Docker Desktop and the current Docker context are supported.

Press `r` in the dev session or run `tako dev --restart` after changing the image or Compose configuration. For live source updates, use a project-local bind mount and a watch command in your Compose service. Development permits writable bind mounts inside the project; production requires read-only binds. Published ports are replaced with private ingress in both modes.

Stopping the app or shutting down the dev daemon stops its containers and preserves named volumes. Each checkout and dev variant has its own Compose project and default named volumes. Reopening the same checkout and variant reuses that data. A daemon crash can leave containers running; starting the same app again reconciles them. Standalone `docker compose up --build --wait` remains available and uses a separate project and its original port mappings.
