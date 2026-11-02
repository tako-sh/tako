---
title: "Docker Compose Updates with Persistent Volumes: Keep Unchanged Services Running"
seoTitle: "Docker Compose Updates with Persistent Volumes"
date: "2026-11-02T03:24"
description: "Learn how Tako updates changed Docker Compose services, preserves named volumes and unchanged containers, and recovers when a deployment fails."
image: da95e218353c
imageAlt: "An octopus swaps a web service cartridge while an adjacent Redis service and its data volume remain connected."
---

You changed the web app. Redis didn't change. A deployment should be able to replace the web container while leaving that healthy Redis container, its network endpoint, and its data alone.

Tako's production Docker Compose support works at that service boundary. It prepares the release, compares the desired services with the running containers, and replaces what changed. Named volumes belong to the app and environment rather than the individual release. Here's how those choices fit together, including what happens when an update fails halfway through.

## A new release doesn't mean a whole new stack

Assume you've already [deployed a Compose app to a VPS](/blog/how-to-deploy-docker-compose-to-a-vps/). Its selected HTTP service is `web`, and it depends on a healthy Redis service. A relevant fragment of the existing `compose.yaml` might look like this:

```yaml
services:
  web:
    build: .
    depends_on:
      redis:
        condition: service_healthy
  redis:
    image: redis:7-alpine
    command: [redis-server, --appendonly, "yes"]
    volumes: [redis-data:/data]
    healthcheck:
      test: [CMD, redis-cli, ping]
      interval: 1s
      retries: 10

volumes:
  redis-data:
```

Keep the web app's existing build, command, environment, and health configuration. This fragment illustrates the dependency and named volume; the [Compose guide](/docs/docker-compose/) covers the full deployment setup and supported production settings.

Before changing containers, Tako captures production inputs into a private directory owned by the service and prepares and pins the images. The first successful preparation of a release is immutable. A retry of that release uses the captured source and images, even if uploaded files or mutable image tags subsequently change. To rebuild or resolve updated images, create a new release.

That matters during recovery. The deployment shouldn't silently become a different application because someone edited a source directory after preparation or moved an image tag. The [preparation implementation](https://github.com/tako-sh/tako/blob/master/tako-compose/src/prepared.rs) retains the first successful source snapshot and image record.

Tako then compares each service's resolved configuration and image runtime contents. Dependency conditions, referenced secrets and resources, and mounted file contents and executable permissions participate in service identity. A different release directory or build attestation alone doesn't require a new container.

The comparison is per service. If only the prepared web image changes, an otherwise unchanged healthy Redis container keeps its ID, endpoint, and volume. If the Redis image or configuration changes too, it becomes an update candidate. Reusing the same tag text isn't sufficient to establish that its image contents are unchanged.

Completed dependency jobs also have identities. An unchanged job that already completed successfully stays completed rather than running again on every web deploy. If a migration job must execute new work, make that change part of its desired generation; don't treat redeploying an unrelated service as an instruction to rerun it.

```d2
direction: right

release: "Prepared release\nchanged web image"
planner: "Compare services"
web: "web\nreplace changed container" {
  style.fill: "#E88783"
}
redis: "Redis\nretain healthy container" {
  style.fill: "#9BC4B6"
}
data: "redis-data\nstable named volume" {
  shape: cylinder
}

release -> planner
planner -> web: "changed"
planner -> redis: "unchanged"
redis -> data
```

## Persistent volumes determine when overlap is safe

The Compose project identity is stable for one app/environment on one server. Named volumes therefore remain the same resources across releases. They survive deployment, stop/start, rollback, and application deletion. Tako preserves the volumes; removing the volumes themselves requires an explicit Docker volume operation. Application and database commands can still change or delete the data inside them.

Keep that identity in mind when testing persistence. A different app name, environment, or server is a different deployment; this isn't an automatic cross-server data replication mechanism. Production Compose deployments target one server.

Declare named mounts for persistent data. Anonymous volumes are rejected, including implicit image volumes without a matching explicit named volume or tmpfs mount. Production writable bind mounts are also rejected. The runtime must know which storage survives container replacement; an accidental anonymous volume is a poor place to discover your database lived.

Preserving a volume is only half the update problem. The other half is deciding whether old and new processes may use it simultaneously:

| Service being replaced | Default order | Reason to inspect the choice |
| --- | --- | --- |
| Selected HTTP service without writable persistent volumes | Start first | Old and new generations overlap during readiness checks. |
| Selected HTTP service with writable persistent volumes | Stop first | Concurrent writers may be unsafe. |
| Workers and other services | Stop first | Job execution and shared resources need explicit overlap decisions. |

These are defaults for changed services, not instructions to stop an unchanged dependency. A retained healthy Redis container stays running while a changed stateless web service follows its start-first path.

You can explicitly choose an order with the standard Compose `deploy.update_config` shape:

```yaml
services:
  web:
    deploy:
      update_config:
        order: start-first
        monitor: 5s
```

Add that fragment to the existing service definition. Tako supports `order` and `monitor`; support for this shape doesn't imply support for every option in the [Compose deploy specification](https://docs.docker.com/reference/compose-file/deploy/). `rollback_config` is unsupported.

Use start-first only when the application can safely run both generations, including their access to data and queues. Leave enough memory for the overlap. Selecting the flag doesn't make a database safe for two writers or make an incompatible migration compatible.

New long-running containers must remain running and pass any Docker health check through the monitor window. The default is five seconds, with supported values from zero seconds through two minutes. Unchanged healthy containers skip that window. The selected HTTP service also passes Tako's HTTP readiness and stability checks before entering public routing.

Dependency conditions retain their meaning: `service_started`, `service_healthy`, and `service_completed_successfully` represent different requirements. A started dependency isn't silently upgraded to a healthy requirement. Define the health checks and conditions your consumers actually need.

Start-first protects traffic through Tako's HTTP ingress during a compatible update. Internal Docker DNS clients can see a candidate before Tako admits it to public routing. A database restart can still interrupt requests, and old HTTP requests drain for up to 30 seconds. Longer requests and persistent connections need application-level reconnect handling. There is no blanket zero-downtime promise for the entire stack.

## Recover containers, then verify the data separately

Tako creates a replacement before stopping a stop-first predecessor, so container creation errors don't unnecessarily stop the working process. If readiness subsequently fails, it removes newly introduced containers and restarts predecessors it stopped. Healthy unchanged services remain running.

Recovery uses the predecessors' original dependency graph and readiness conditions. It doesn't reinterpret their startup requirements through the failed release's configuration. This also preserves completed dependency jobs. If recovery itself fails, the application reports an error rather than claiming the old deployment is healthy.

Process interruption has a durable boundary too:

| Interruption | Authoritative deployment on recovery |
| --- | --- |
| Before the new deployment commits | Previous committed release |
| After the new deployment commits | New committed release |

Tako records a pending marker before changing containers. The active release, routes, replica overrides, and encrypted credentials commit together. On startup, the server reconciles that committed release, adopts matching containers, and cleans abandoned generations. A normal Tako server shutdown leaves production Compose containers running so startup can adopt them. That doesn't make the public proxy available while its own server process is stopped.

The [rollout implementation](https://github.com/tako-sh/tako/blob/master/tako-compose/src/rollout.rs) and [deployment transaction](https://github.com/tako-sh/tako/blob/master/tako-server/src/operations/compose.rs) encode those recovery steps. Prepared deployment inputs live outside the replaceable app-data tree, and older release files remain protected while containers reference them.

For a controlled update test, write a recognizable record through your app, record the Redis container's identity, and deploy a web-only change. Then inspect the deployment:

```bash
tako deploy --env production
tako status --json
tako releases list --env production --json
tako logs --env production --days 1 --json
```

Request the public hostname, verify the new web behavior, read the existing record, and compare Redis's container identity in Docker. In a separate test environment, make the replacement fail readiness and verify the restored application and data. The [operations guide](/docs/operations/) covers finite status, logs, and release inspection. Service replica changes have their own [scaling walkthrough](/blog/scale-docker-compose-services-on-a-vps/).

Finally, application rollback restores containers, not database contents or schema. Docker named volumes are outside [Tako's app-data backups](/docs/backups/#container-data). Use database-native dumps or a coordinated volume backup and verify restoration into a separate stack. A successful code rollback cannot undo a destructive migration.

The useful outcome is specific: unchanged healthy services retain their containers, changed services follow an explicit replacement order, and the committed release guides recovery. Keep data recovery as a separate, tested procedure, and a small Compose stack stays understandable when the next deploy gets interesting.
