---
title: Docker Compose
description: "Deploy an existing Compose stack to one VPS with persistent named volumes, Tako HTTPS, service logs, status, and application rollback."
---

A Compose project is an **application group**. Each service with an `x-tako` mapping is a Tako app with its own routes, replicas, readiness and backend pool. Services without `x-tako`, such as databases and workers, remain ordinary Docker services. The group owns deployment, rollback, shared networks and volumes. A Tako SDK is optional.

## Configure a stack

Keep `compose.yaml` and its build contexts in the application directory. Group settings stay in `tako.toml`:

```toml tako.toml
name = "my-stack"

[compose]
file = "compose.yaml"

[envs.production]
servers = ["prod"]
```

Put each app's route beside its Compose service:

```yaml compose.yaml
services:
  web:
    image: example/web:1
    x-tako:
      envs:
        production:
          route: example.com
  api:
    image: example/api:1
    x-tako:
      envs:
        production:
          routes: [api.example.com, example.com/api/*]
  redis:
    image: redis:7-alpine
    volumes: [redis-data:/data]
volumes:
  redis-data:
```

The web and API images must declare their listening port with `EXPOSE`, or the services must declare it with standard Compose `expose` or `ports`. Tako selects a single TCP port from `expose`, then `ports` targets, then image metadata. It never guesses between multiple candidates: use one `expose` entry to select the HTTP port. Host publications are replaced with private loopback ports. There is no `x-tako.port` or Tako health-path setting.

Readiness uses the container's Docker health check when present, plus a connection check on the app's private port. Define application readiness with standard Compose `healthcheck` and dependency ordering with `depends_on`. Without a Docker health check, a running container with an accepting socket is considered ready; an arbitrary HTTP `/` response is not probed.

`x-tako.envs.<environment>` accepts `route` or `routes` and optional `source_ip`, which inherits the group setting when omitted. Routes use the same host/path syntax as native apps, must be literal, and cannot be assigned to two apps in the same group. Unknown `x-tako` fields fail validation. An empty `x-tako: {}` still opts a service into being a Tako app; without public routes it has only a private backend. Removing the mapping returns it to ordinary Compose management.

Group settings in `tako.toml` include identity, environment/server selection, variables and secrets, certificate provider/credentials, storage bindings and backups. Compose remains authoritative for builds, commands, container environment, health checks, resource limits and replica defaults. SDK bootstrap and native scale-to-zero are not enabled by adding `x-tako`.

A complete runnable [web/API/Redis example](https://github.com/tako-sh/tako/tree/master/examples/compose/web-api-redis) includes the frontend, API, Dockerfile, local routes and persistence verification.

Existing configurations must move their routes from `[envs.*]` into the selected service's `x-tako.envs.*`, remove `[compose].service`, `port`, `health_path` and `health_host`, and declare exposure/readiness through Compose or the image. Deploy with the matching CLI and server versions; protocol v0 does not keep the previous schema.

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

Tako replaces Compose port publications. Only Tako apps are published, with an assigned loopback port per replica; Tako handles public HTTPS and routes. Databases, caches, and workers communicate through Compose networks and service names.

Use named volumes for persistent data. The project identity is stable for one app/environment on one server, so new releases reuse the same volumes. Anonymous volumes are rejected in production. If an image declares a volume, mount a named volume there, or explicitly use tmpfs for disposable data. Read-only bind mounts and config/secret files must stay inside the uploaded app directory. Tako retains older release files while containers reference them.

Networks, volumes, configs, and secrets are scoped to the app/environment. External resources and custom resource names are rejected in production. Networks use Docker's bridge driver and volumes use its local driver; driver options and custom IPAM settings are unsupported.

Named volumes survive deployment, stop/start, rollback, and application deletion. Deleting data requires an explicit Docker volume operation. Tako's app-data backups do **not** include Docker named volumes. Use database-native dumps or a coordinated volume backup, and verify restoration into a separate stack. Application rollback restores the previous containers using their original dependency conditions. Completed dependency jobs stay completed. Rollback does not reverse schema migrations or restore database contents.

## Updates and recovery

Tako prepares and pins all images before changing running containers. It compares each service's image configuration and filesystem, plus its resolved Compose configuration, dependency conditions, referenced secrets, and mounted file contents and executable permissions. Build attestations and release directory names do not trigger replacements. Unchanged services keep their container IDs, network endpoints, and volumes. Updating a Bun web app therefore leaves unchanged Redis running.

| Service                                      | Default update order                                                     |
| -------------------------------------------- | ------------------------------------------------------------------------ |
| Tako app without writable persistent volumes | Start replacement, verify readiness, switch traffic, drain old container |
| Tako app with writable persistent volumes    | Stop old container before starting its replacement                       |
| Workers and other services                   | Stop old container before starting its replacement                       |

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

Use `start-first` only when old and new containers can safely overlap, including access to shared data or job queues. Allocate enough memory for both generations. `monitor` defaults to 5 seconds and supports 0 seconds through 2 minutes. New containers must remain running and pass any Docker health check for this window; unchanged healthy containers skip it. Tako also checks each app's private connection and stability window before routing to it. Dependencies follow `service_started`, `service_healthy`, and `service_completed_successfully`; unchanged completed jobs are not rerun.

If readiness fails, Tako removes the new containers and restarts any predecessors it stopped. Healthy unchanged containers remain running. Failed recovery is reported as an application error. The active release, routes, replica overrides, stopped state, and encrypted credentials commit together. On a server restart, Tako reconciles that committed release, adopts matching containers, and removes abandoned generations. A normal Tako server shutdown leaves production Compose containers running for adoption on startup.

Start-first updates protect traffic through Tako's HTTP ingress. Updates to databases or other dependencies can still cause downtime, and migrations must remain compatible with the versions that overlap. Internal clients using Docker DNS can see a candidate before Tako admits it to public HTTP routing. Existing requests and server-sent event streams drain for up to 30 seconds; upgraded WebSocket connections do not hold the drain and are closed when the old container stops, so clients need reconnect handling.

## Scale individual services

```bash
# With exactly one Tako app, --service can be omitted.
tako scale 5 --env production

# Scale a named service without duplicating its dependencies.
tako scale 5 --service api --env production
tako scale 3 --service worker --env production

# Clear that service's override and use its Compose default.
tako scale --service api --reset --env production
```

With multiple tagged apps, `tako scale` requires `--service`; Tako never chooses an arbitrary app. `deploy.replicas` supplies each service's default (one when omitted). A successful `tako scale` stores an override for that app, environment, server, and service. Overrides survive deployments, release rollbacks, secret rotation, and server restarts. Changing `deploy.replicas` takes effect for services without an override; use `--reset` to follow the current release's default again. A failed scale restores the previous count and leaves the previous override intact. Removing a service in a successful deployment removes its override; deleting the app clears all overrides.

Scaling or resetting a stopped app or group changes the count used on its next start. It does not resume the app.

Every HTTP replica has an independent health check and routing entry. Tako balances traffic across healthy replicas and drains requests before removing replicas. Scaling `api` to five creates five API containers in the same project; Redis stays at its own count and keeps its existing container and data. Replicas share the service's networks and named volumes. Scale only services whose processes can safely share those resources; a replica count does not configure database replication.

Production Compose deployments target one server. Each Tako app has a configured count of 1–128 replicas; ordinary Docker services support 0–128, and required dependencies must have at least one configured replica. An explicitly stopped app or group runs zero replicas without changing those counts. Compose uses these explicit per-service counts, including on a standby server, rather than the native process CPU-based maximum. HTTP scale-to-zero and automatic cold starts are unsupported for Compose.

`tako status --json` identifies the deployment as `kind: "application_group"`. Its `applications` list contains each tagged app's identity (`<group>/<environment>/<service>`), state, routes and instances; `services` reports Docker containers, and `replica_overrides` records overrides. Stopped apps remain in `applications` with `state: "stopped"` and no instances. Human output labels each service as an app or Docker service. Log prefixes include the service name and generation.

Production rejects writable bind mounts, outside build contexts, `container_name`, `network_mode`, profiles, links, `volumes_from`, shared service namespaces, Compose lifecycle hooks, optional dependencies, and `depends_on.restart`. `update_config` supports `order` and `monitor`; `rollback_config` is unsupported. These restrictions keep replacement and recovery behavior explicit.

Host privileges and resource sharing are unsupported: this includes `privileged`, added capabilities, devices, host namespaces, custom runtimes, Docker socket injection, and providers. `security_opt` supports only `no-new-privileges`. Logging uses `json-file`, `local`, or `none`, with `max-size`, `max-file`, and `compress` options. Builds reject SSH forwarding, entitlements, host networking, and cache import/export settings. Compose `include` and `extends` are unsupported. Unrecognized production service and build settings fail validation before Docker creates containers.

## Start and stop apps

```bash
# Stop only the API; the frontend and database keep running.
tako stop --service api --env production
tako status --json

# Set the next start's count without resuming the API.
tako scale 3 --service api --env production
tako start --service api --env production

# Stop or start the whole group, including ordinary Docker services.
tako stop --env production
tako start --env production
```

Only services with `x-tako` support individual start and stop. Routes remain configured while an app is stopped, but requests have no backend and do not wake it. Named volumes and configured replica counts are preserved. Stop intent is separate from scaling and survives deployments, rollbacks, and server restarts.

Starting the group resumes its ordinary Docker services and apps, except apps that were individually stopped. Start the group before resuming one of those apps. Explicit stops take precedence over `depends_on`: starting or redeploying a consumer does not resume a stopped dependency or wait for it to become healthy. Consumers must handle that dependency being unavailable.

Deployment, rollback, and deletion belong to the group. `tako delete --service api` is rejected. To remove a service, edit the Compose file and redeploy; to remove the whole deployment, use `tako delete --env production`. Docker named volumes remain in either case.

## Verify and develop locally

Request the deployed hostname, write a record or upload through the app, deploy again, and confirm that the data remains. Also test a service failure and restore a backup into a separate environment.

Run `tako dev` from the app directory. Tako builds and starts the whole group and waits for Docker health and app connection checks. Each app gets `<service>.<group>.tako` (or the local development suffix), including apps without public production routes. Override these names with `x-tako.envs.development.route` or `routes`; development variants keep their own hostnames. Docker Engine and Compose must be available locally; Docker Desktop and the current Docker context are supported. Development runs one replica of each tagged app on its own assigned local port, regardless of production `deploy.replicas` or server-side overrides; other services retain their Compose counts.

Press `r` in the dev session or run `tako dev --restart` after changing the image or Compose configuration. For live source updates, use a project-local bind mount and a watch command in your Compose service. Development permits writable bind mounts inside the project; production requires read-only binds. Published ports are replaced with private ingress in both modes.

Stopping the app or shutting down the dev daemon stops its containers and preserves named volumes. Each checkout and dev variant has its own Compose project and default named volumes. Reopening the same checkout and variant reuses that data. A daemon crash can leave containers running; starting the same app again reconciles them. Standalone `docker compose up --build --wait` remains available and uses a separate project and its original port mappings.

## Native private networking

On a native network, supported Compose services use Tako's private gateway and DNS resolver for `{app}.{env}.{network-id}.tako.internal` addresses. The gateway admits current trusted placements and provides DNS independently of host systemd-resolved. Use `tako connect` to resolve private names from workstation applications. See [Native networks](/networks/#use-private-service-addresses).

Compose deployments still target one server. Network membership and server drain do not replicate named volumes, move databases, or switch global ingress. Preserve separate backup and migration procedures for stateful services.
