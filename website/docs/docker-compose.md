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

Reference secrets explicitly in Compose, for example `POSTGRES_PASSWORD: ${DATABASE_PASSWORD:?Set DATABASE_PASSWORD}`. Tako supplies app variables and decrypted secrets to Compose interpolation. It does not write interpolated configuration or secrets into release artifacts. Docker's own container metadata contains the environment values that the Compose file assigns.

Client controls are reserved: `DOCKER_*`, `COMPOSE_*`, `BUILDX_*`, `BUILDKIT_*`, `EXPERIMENTAL_BUILDKIT_SOURCE_POLICY`, loader variables, `PATH`, `HOME`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, and proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` and their lowercase forms). Docker and Buildx configuration comes from the service account, outside the captured app directory. To configure a container's proxy or CA settings, use explicit service `environment` entries, optionally referencing app variables with different names.

Production does not load an implicit `.env` file. Use Tako variables and secrets for interpolation, or an explicit project-local service `env_file` for container environment values. File references must be literal relative paths; interpolation in file paths is unsupported.

Before invoking Docker, Tako copies production inputs into a service-owned private directory and validates the Compose model and every build input. Builds, restarts, secret rotation, and rollback use that captured source and pinned images. These deployment inputs stay outside the app-data tree, so restoring an app-data backup preserves them.

The first successful preparation of a release is immutable. Preparing that release again reuses its captured source and images, even if uploaded files or image tags have changed; deploy a new release to rebuild or pull updated images.

Decrypted interpolation values remain in memory. Rotating an environment-backed Compose secret or config replaces its consuming services so their injected files receive the new value.

Build contexts, Dockerfiles, additional file contexts, build secrets, runtime configs/secrets, and bind sources must stay inside the app directory. Additional `service:` build contexts are supported. Source must contain only regular files and directories, without symlinks or hard links, and fit within 2 GiB, 100,000 entries, and 128 directory levels. The Compose file is limited to 16 MiB. The root `logs` directory is reserved for runtime output and excluded from preparation.

### HTTP health checks for JavaScript services

If the runtime image includes `tako.sh` as an installed production dependency,
use its local health check command. For a Bun service:

```yaml
healthcheck:
  test: ["CMD", "bun", "./node_modules/.bin/tako.sh", "healthcheck"]
  interval: 5s
  timeout: 3s
  retries: 10
```

For Node.js 20+, use
`test: ["CMD", "./node_modules/.bin/tako.sh", "healthcheck"]`.
Install the package during the image build and retain it in the runtime image;
these commands run locally without fetching packages. A bundled app without
`node_modules` must include the package separately to use this command.

Pass a URL after `healthcheck` or set `TAKO_HEALTHCHECK_URL` in the service's
`environment`. The default is `http://127.0.0.1:${PORT:-3000}/health`.
`--timeout <ms>` overrides `TAKO_HEALTHCHECK_TIMEOUT_MS`, which defaults to
2000 milliseconds. The command makes one GET request, accepts only HTTP 2xx,
does not follow redirects or read the body, and exits 1 with a short error on
failure. Docker controls retries and intervals. For an ad hoc check, run
`npx tako.sh healthcheck <url>`.

## Networking and persistence

Tako replaces Compose port publications. Only the selected HTTP service is published, with an assigned loopback port per replica; Tako handles public HTTPS and routes. Databases, caches, and workers communicate through Compose networks and service names.

Use named volumes for persistent data. The project identity is stable for one app/environment on one server, so new releases reuse the same volumes. Anonymous volumes are rejected in production. If an image declares a volume, mount a named volume there, or explicitly use tmpfs for disposable data. Read-only bind mounts and config/secret files must stay inside the uploaded app directory. Tako retains older release files while containers reference them.

Networks, volumes, configs, and secrets are scoped to the app/environment. External resources and custom resource names are rejected in production. Networks use Docker's bridge driver and volumes use its local driver; driver options and custom IPAM settings are unsupported.

Named volumes survive deployment, stop/start, rollback, and application deletion. Deleting data requires an explicit Docker volume operation. Tako's app-data backups do **not** include Docker named volumes. Use database-native dumps or a coordinated volume backup, and verify restoration into a separate stack. Application rollback restores the previous containers using their original dependency conditions. Completed dependency jobs stay completed. Rollback does not reverse schema migrations or restore database contents.

## Updates and recovery

Tako prepares and pins all images before changing running containers. It compares each service's image configuration and filesystem, plus its resolved Compose configuration, dependency conditions, referenced secrets, and mounted file contents and executable permissions. Build attestations and release directory names do not trigger replacements. Unchanged services keep their container IDs, network endpoints, and volumes. Updating a Bun web app therefore leaves unchanged Redis running.

| Service                                                   | Default update order                                                     |
| --------------------------------------------------------- | ------------------------------------------------------------------------ |
| Selected HTTP service without writable persistent volumes | Start replacement, verify readiness, switch traffic, drain old container |
| Selected HTTP service with writable persistent volumes    | Stop old container before starting its replacement                       |
| Workers and other services                                | Stop old container before starting its replacement                       |

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

If readiness fails, Tako removes the new containers and restarts any predecessors it stopped. Healthy unchanged containers remain running. Failed recovery is reported as an application error. The active release, routes, replica overrides, and encrypted credentials commit together. On a server restart, Tako reconciles that committed release, adopts matching containers, and removes abandoned generations. A normal Tako server shutdown leaves production Compose containers running for adoption on startup.

Start-first updates protect traffic through Tako's HTTP ingress. Updates to databases or other dependencies can still cause downtime, and migrations must remain compatible with the versions that overlap. Internal clients using Docker DNS can see a candidate before Tako admits it to public HTTP routing. Existing requests and server-sent event streams drain for up to 30 seconds; upgraded WebSocket connections do not hold the drain and are closed when the old container stops, so clients need reconnect handling.

## Scale individual services

```bash
# Scale the HTTP service selected by [compose].service.
tako scale 5 --env production

# Scale a named service without duplicating its dependencies.
tako scale 5 --service api --env production
tako scale 3 --service worker --env production

# Clear that service's override and immediately apply its Compose default.
tako scale --service api --reset --env production
```

`deploy.replicas` supplies each service's default (one when omitted). A successful `tako scale` stores an override for that app, environment, server, and service. Overrides survive deployments, release rollbacks, secret rotation, and server restarts. Changing `deploy.replicas` takes effect for services without an override; use `--reset` to follow the current release's default again. A failed scale restores the previous count and leaves the previous override intact. Removing a service in a successful deployment removes its override; deleting the app clears all overrides.

Every HTTP replica has an independent health check and routing entry. Tako balances traffic across healthy replicas and drains requests before removing replicas. Scaling `api` to five creates five API containers in the same project; Redis stays at its own count and keeps its existing container and data. Replicas share the service's networks and named volumes. Scale only services whose processes can safely share those resources; a replica count does not configure database replication.

Production Compose deployments target one server. The HTTP service supports 1–128 replicas; other services support 0–128, and required dependencies must keep at least one replica. Compose uses these explicit per-service counts, including on a standby server, rather than the native process CPU-based maximum. HTTP scale-to-zero and automatic cold starts are unsupported for Compose. The management `stop` command stops the whole stack; `tako delete` removes the deployment while preserving named volumes.

`tako status --json` includes original service names, state, health, exit codes, and `replica_overrides`. Log prefixes include the service name and generation.

Production rejects writable bind mounts, outside build contexts, `container_name`, `network_mode`, profiles, links, `volumes_from`, shared service namespaces, Compose lifecycle hooks, optional dependencies, and `depends_on.restart`. `update_config` supports `order` and `monitor`; `rollback_config` is unsupported. These restrictions keep replacement and recovery behavior explicit.

Host privileges and resource sharing are unsupported: this includes `privileged`, added capabilities, devices, host namespaces, custom runtimes, Docker socket injection, and providers. `security_opt` supports only `no-new-privileges`. Logging uses `json-file`, `local`, or `none`, with `max-size`, `max-file`, and `compress` options. Builds reject SSH forwarding, entitlements, host networking, and cache import/export settings. Compose `include` and `extends` are unsupported. Unrecognized production service and build settings fail validation before Docker creates containers.

## Verify and develop locally

Request the deployed hostname, write a record or upload through the app, deploy again, and confirm that the data remains. Also test a service failure and restore a backup into a separate environment.

Run `tako dev` from the app directory. Tako builds and starts the whole Compose stack, waits for service and HTTP health, and serves the selected service through the usual local HTTPS hostname. Docker Engine and Compose must be available locally; Docker Desktop and the current Docker context are supported. Development runs one HTTP replica on its assigned local port, regardless of production `deploy.replicas` or server-side overrides; other services retain their Compose counts.

Press `r` in the dev session or run `tako dev --restart` after changing the image or Compose configuration. For live source updates, use a project-local bind mount and a watch command in your Compose service. Development permits writable bind mounts inside the project; production requires read-only binds. Published ports are replaced with private ingress in both modes.

Stopping the app or shutting down the dev daemon stops its containers and preserves named volumes. Each checkout and dev variant has its own Compose project and default named volumes. Reopening the same checkout and variant reuses that data. A daemon crash can leave containers running; starting the same app again reconciles them. Standalone `docker compose up --build --wait` remains available and uses a separate project and its original port mappings.

## Native private networking

On a native network, supported Compose services use Tako's private gateway and DNS resolver for `{app}.{env}.{network-id}.tako.internal` addresses. The gateway admits current trusted placements and provides DNS independently of host systemd-resolved. Use `tako connect` to resolve private names from workstation applications. See [Native networks](/networks/#use-private-service-addresses).

Compose deployments still target one server. Network membership and server drain do not replicate named volumes, move databases, or switch global ingress. Preserve separate backup and migration procedures for stateful services.
