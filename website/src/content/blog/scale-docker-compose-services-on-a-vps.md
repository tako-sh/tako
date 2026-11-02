---
title: "How to Scale Docker Compose Services on a VPS with Tako"
date: "2026-11-02T03:25"
description: "Scale Docker Compose web and worker services on one VPS with Tako, preserve independent dependencies, and manage persistent replica overrides."
image: 0285e241f423
imageAlt: "A coral octopus installs matching web modules above worker modules and a shared database in one server cabinet."
---

Your Compose app has a web service, a worker, and Redis. The web service needs more capacity. Redis is fine. Doubling the entire stack would be a rather enthusiastic response to a smaller problem.

Tako can scale individual Compose services on one VPS. Each service keeps its own replica count, while the selected HTTP service gets health checks and public routing for every replica. We'll use a small stack to make the change visible, then explain why that count survives your next deploy and how to return to the Compose default.

## Start with services that can run more than once

This tutorial assumes you have a Tako server with Docker Engine and the Compose plugin configured, plus a public app hostname. Follow [the Compose deployment tutorial](/blog/how-to-deploy-docker-compose-to-a-vps/) for initial setup. The [Compose reference](/docs/docker-compose/) covers the supported production subset and Docker access requirements.

For a disposable demonstration, create this `compose.yaml`:

```yaml
services:
  web:
    image: docker.io/library/busybox:1.37
    command:
      - sh
      - -c
      - mkdir -p /www; hostname > /www/index.html; exec httpd -f -p 8080 -h /www
    deploy:
      replicas: 2
    healthcheck:
      test: [CMD-SHELL, "wget -q -O /dev/null http://127.0.0.1:8080/"]
      interval: 1s
      timeout: 2s
      retries: 3
  redis:
    image: docker.io/library/redis:7-alpine
    command: [redis-server, --appendonly, "yes"]
    volumes:
      - redis-data:/data
    healthcheck:
      test: [CMD, redis-cli, ping]
      interval: 1s
      timeout: 2s
      retries: 30
  worker:
    image: docker.io/library/redis:7-alpine
    tmpfs: [/data]
    entrypoint: [sh, -c]
    command: ["while true; do redis-cli -h redis INCR jobs >/dev/null; sleep 1; done"]
    depends_on:
      redis:
        condition: service_healthy
    deploy:
      replicas: 1
volumes:
  redis-data:
```

The web process returns its container hostname, which helps distinguish replicas when you request the public route. The worker increments a Redis counter once per loop. It is a scaling probe, not a production job queue: adding workers increases concurrent increments, and the example promises no job delivery semantics. Its command comes from Tako's [web-and-worker fixture](https://github.com/tako-sh/tako/blob/master/e2e/fixtures/compose/web-worker/compose.yaml).

Redis owns the named data volume. Worker containers use disposable tmpfs at `/data`, making that image's data mount explicit. The web processes keep only disposable files in their container filesystems. None of these processes needs exclusive access to the same writable file to perform its demonstration task.

Create `tako.toml`, replacing the hostname and inventory name with your actual deployment target:

```toml
name = "compose-scale-demo"

[compose]
file = "compose.yaml"
service = "web"
port = 8080
health_path = "/"

[envs.production]
route = "app.example.com"
servers = ["prod"]
```

The `[compose].service` field selects the HTTP ingress. Tako assigns each HTTP replica a loopback port and routes public HTTPS through its proxy. Redis and the worker use Compose networking. You don't need to publish their ports. See the [configuration reference](/docs/tako-toml/) for Compose fields.

Deploy and inspect the result:

```bash
tako deploy --env production
tako status --json
curl -fsS https://app.example.com/
```

Initially, the file asks for two web replicas and one worker. Redis defaults to one because it omits `deploy.replicas`. Docker documents that setting in its [Compose deployment specification](https://docs.docker.com/reference/compose-file/deploy/); Tako applies its own validation and lifecycle rules around those counts.

## Scale the web service and worker independently

From the project directory, increase HTTP capacity:

```bash
tako scale 4 --env production
```

Without `--service`, the target is `web`, because that's the HTTP service selected in `tako.toml`. The explicit equivalent is:

```bash
tako scale 4 --service web --env production
```

Then increase the worker count separately:

```bash
tako scale 3 --service worker --env production
```

The resulting counts are independent:

| Service | Compose default | Applied override | Running target |
| --- | --- | --- | --- |
| `web` | 2 | 4 | 4 |
| `worker` | 1 | 3 | 3 |
| `redis` | 1 | None | 1 |

Scaling the web service does not create four copies of Redis. Unchanged dependencies keep their containers, network endpoints, and data. The [server scaling tests](https://github.com/tako-sh/tako/blob/master/tako-server/src/tests/compose_scaling.rs) verify retained container identities alongside HTTP replica health and routing.

```d2
direction: right

visitors: "Visitors"
proxy: "Tako HTTPS proxy" {
  style.fill: "#E88783"
}
vps: "One VPS / one Compose project" {
  web: "4 healthy web replicas" {
    style.fill: "#9BC4B6"
  }
  worker: "3 worker replicas"
  redis: "1 Redis container"
  volume: "Persistent named volume"
  worker -> redis: "private service network"
  redis -> volume
}
visitors -> proxy: "app.example.com"
proxy -> vps.web: "per-replica routing"
```

Every HTTP replica must pass readiness and stability checks before Tako admits it to public routing. When reducing the count, Tako removes retiring replicas from new request routing and drains existing requests before removing containers. The [deployment documentation](/docs/deployment/) explains how health checks protect a release; Compose uses its own container rollout underneath that public routing boundary.

Inspect status and recent logs after each change:

```bash
tako status --json
tako logs --env production --days 1 --json
```

Find the original service names, container state, health, and `replica_overrides`. Request the web route several times to observe the hostname responses. Use both status and requests: one successful response establishes that one path worked, rather than proving every replica is healthy.

Extra replicas still share one machine's CPU, memory, disk, networks, and named volumes. A count increase cannot create more VPS resources. Apply this to stateless web processes and workers whose concurrency behavior you understand. Three Postgres containers sharing a data directory would not become a replicated database. Replica counts don't configure database replication.

## Keep overrides deliberate across releases

Successful scaling records an override for the app, environment, server, and service. It survives a deploy, release rollback, secret rotation, and server restart. That matters during recovery: rolling back application code should not unexpectedly undo the capacity you just added.

Suppose you edit the web service's `deploy.replicas` from `2` to `3` and deploy again. The existing override of `4` still wins. The new file defines the default, while the server retains your explicit operational choice.

Clear the web override to apply the current release's default immediately:

```bash
tako scale --service web --reset --env production
```

After that reset, the web count is three in our edited example. Future releases follow their defaults until you set another override. Clear the worker override independently:

```bash
tako scale --service worker --reset --env production
```

Supply either a count or `--reset`; combining them is invalid. The [CLI reference](/docs/cli/) documents both forms. Removing a service in a successful deployment removes its override, and deleting the app clears its overrides.

The production limits are explicit:

| Service category | Supported replica count |
| --- | --- |
| Selected HTTP service | 1–128 |
| Other services | 0–128 |
| Required dependency | At least one |

An independent worker can therefore be paused with `tako scale 0 --service worker --env production`. Redis cannot be reduced to zero in this example because the worker declares it as a required dependency. HTTP scale-to-zero and automatic cold starts are unsupported for Compose. Production Compose deployments target one server.

Scaling follows the same durable commit and recovery boundary as deployment. If readiness or the scale commit fails, Tako restores the preceding containers and count while retaining the previous override. If recovery itself fails, it reports an application error; inspect status and logs instead of assuming the requested count took effect. The [Compose operations implementation](https://github.com/tako-sh/tako/blob/master/tako-server/src/operations/compose.rs) handles that boundary.

Persistent counts and persistent data solve different problems. Named volumes survive the container lifecycle, but increasing replicas does not make shared writes safe or create a backup. Read [Compose updates with persistent volumes](/blog/docker-compose-updates-with-persistent-volumes/) before applying this workflow to stateful services.

Start with a small count change, verify the selected service, and check that dependencies stayed at their own counts. When capacity should follow the release file again, reset the override. One VPS, independent service counts, and an explicit way back to your defaults.
