---
title: "How to Deploy Docker Compose to a VPS with Automatic HTTPS"
date: "2026-11-02T03:23"
description: "Deploy a Bun and Redis Docker Compose stack to one VPS with Tako: local HTTPS, private networking, named volumes, and automatic production TLS."
image: 0d360d0553c5
imageAlt: "A coral octopus opens a secure web-service dock for visitors while Redis stays connected behind the gate."
---

> **Networking update.** The Tailscale and NetBird integrations and older setup commands described here have been retired. For new installations, see [native WireGuard networks](/blog/native-wireguard-networks-for-vps-apps/). For an existing Tailscale or NetBird host, follow the [migration guide](/blog/migrate-tailscale-to-tako-wireguard/).

Your app already has a Compose file: one web service, a Redis dependency, and a named volume. You want to deploy that stack to a VPS, give it a real HTTPS hostname, and keep its data when you ship the next version.

Tako now supports that shape directly. We'll deploy the shipped Bun-and-Redis example, test its counter through local HTTPS, then verify the same behavior on a public domain. The app uses its existing command and needs no Tako SDK.

## Run the Bun and Redis stack locally

Start with the [complete example](https://github.com/tako-sh/tako/tree/master/examples/compose/bun-redis). Copy it into its own directory so your application files are easy to inspect:

```bash
git clone --depth 1 https://github.com/tako-sh/tako.git tako-examples
cp -R tako-examples/examples/compose/bun-redis ./bun-redis
cd bun-redis
```

Keep the copied `.dockerignore`, Dockerfile, `server.ts`, Compose file, and `tako.toml`. The Bun server handles two routes: `/` increments a Redis counter and returns JSON containing `visits`; `/health` checks Redis without incrementing anything. There is no dependency install or SDK integration to add.

Its `compose.yaml` is:

```yaml
services:
  web:
    build: .
    restart: unless-stopped
    ports:
      - "8080:3000"
    environment:
      REDIS_URL: redis://redis:6379
    depends_on:
      redis:
        condition: service_healthy
    healthcheck:
      test:
        - CMD
        - bun
        - -e
        - "process.exit((await fetch('http://127.0.0.1:3000/health')).ok ? 0 : 1)"
      interval: 5s
      timeout: 3s
      retries: 10
  redis:
    image: docker.io/library/redis:7-alpine
    restart: unless-stopped
    command: [redis-server, --appendonly, "yes"]
    volumes:
      - redis-data:/data
    healthcheck:
      test: [CMD, redis-cli, ping]
      interval: 5s
      timeout: 3s
      retries: 10

volumes:
  redis-data:
```

The app connects to Redis through its service name. `depends_on` waits for Redis's health check, and the web health check exercises the app's Redis connection. Docker's [service reference](https://docs.docker.com/reference/compose-file/services/) describes these dependency and health-check settings; Tako's [Compose guide](/docs/docker-compose/) defines the supported production subset.

The Dockerfile builds the web image from the included server:

```dockerfile
FROM oven/bun:1-alpine
WORKDIR /app
COPY --chown=bun:bun server.ts ./server.ts
USER bun
EXPOSE 3000
CMD ["bun", "server.ts"]
```

Install the Tako CLI if needed:

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

With Docker Engine and Compose 2.24.4 or newer available locally, run:

```bash
docker compose version
docker info
tako dev
```

Tako builds and starts the whole stack, waits for health, and serves the web service through its local HTTPS hostname. Open the URL it reports and request `/` twice: the `visits` values should increase. Request `/health` to check the dependency without changing the counter.

Press `r` in the dev session, or run `tako dev --restart`, after changing the image or Compose configuration. Stopping and restarting the app preserves named volumes. Each checkout and dev variant has its own Compose project, so a different checkout starts with separate data. See [Local development](/docs/development/) for the surrounding HTTPS setup.

For a standalone Compose check, `docker compose up --build --wait` uses the original `8080:3000` mapping. Tako replaces that mapping with private ingress in its own development project. Pick one workflow for your test and use its corresponding URL.

## Prepare the VPS and select the HTTP service

Use a supported Linux VPS with glibc, systemd, and Tako's required isolation controllers. Connect your workstation and server through Tailscale or NetBird, allowing SSH and private management TCP `9844`. Follow [Servers](/docs/servers/) for host requirements and enrollment.

For example, on an existing NetBird network:

```bash
tako servers add ubuntu@my-server.netbird.cloud --vpn netbird --name prod
```

Replace the administrator and hostname with your actual values. Interactive setup offers to install or repair Tako when needed. A Tailscale setup uses its own server hostname and `--vpn tailscale` instead.

**Install Docker Engine and the Compose plugin separately on the VPS.** Compose deployments require Docker and Compose **2.24.4 or newer**. The Tako installer currently installs Podman for standalone image releases; that does not satisfy the Compose requirement. Use [Docker's distribution-specific installation instructions](https://docs.docker.com/engine/install/).

The `tako-server` service account must be able to access the local Docker socket. For a conventional rootful Docker installation with the `docker` group, an administrator can configure a fresh host like this:

```bash
sudo usermod -aG docker tako
sudo systemctl restart tako-server
sudo -u tako docker compose version
sudo -u tako docker info
```

The restart lets the service acquire its new group membership. Docker documents that this group grants root-level privileges in its [post-installation guide](https://docs.docker.com/engine/install/linux-postinstall/). Check access as `tako`, rather than relying on a successful `docker info` from your administrator account.

Back in the app directory, edit the example's `tako.toml`:

```toml
name = "bun-redis"

[compose]
file = "compose.yaml"
service = "web"
port = 3000
health_path = "/health"

[envs.production]
route = "app.example.com"
servers = ["prod"]
```

`service` selects the HTTP ingress service; `port` is that service's container port. It is `3000`, not the standalone host port `8080`. The health path must return HTTP 2xx or 3xx. If your own app restricts Host headers, configure `health_host` to a hostname it accepts.

Point your public domain at the VPS's public address and allow public HTTP `80` and HTTPS `443`. For this direct-to-VPS example, Tako's default Let's Encrypt mode uses HTTP-01 validation. Declaring a route does not create DNS records; [Routing and TLS](/docs/routing/) covers the certificate choices.

Public traffic and private service traffic take different paths:

```d2
direction: right
browser: "Browser"
vps: "One VPS" {
  proxy: "Tako\npublic HTTPS" {style.fill: "#E88783"}
  web: "Bun web\ncontainer port 3000" {style.fill: "#9BC4B6"}
  redis: "Redis\nCompose network"
  volume: "redis-data\nnamed volume"
  proxy -> web: "assigned loopback port"
  web -> redis: "redis:6379"
  redis -> volume
}
browser -> vps.proxy: "app.example.com"
```

Tako replaces Compose port publications and exposes only the selected HTTP service through assigned loopback ports. Redis stays on the Compose network; visitors reach the app through Tako's HTTPS route.

## Deploy and verify the counter and its data

Deploy from the copied application directory:

```bash
tako deploy --env production
tako status --json
tako logs --env production --days 1 --json
```

Tako captures and validates the production inputs, prepares the images, and starts the services. Once the selected web service passes readiness checks, Tako admits it to public traffic. The [deployment guide](/docs/deployment/) covers release preparation and failure handling.

Request the real domain, substituting yours below:

```bash
curl --fail --silent --show-error https://app.example.com/health
curl --fail --silent --show-error https://app.example.com/
curl --fail --silent --show-error https://app.example.com/
```

Confirm a trusted certificate, a successful health response, and increasing `visits` values. Record the observed counter, make a small change to the greeting in `server.ts`, and deploy again. Request `/` afterward: it should contain the new greeting and continue beyond the earlier counter, rather than starting from empty data. We're checking both changed application code and preserved Redis state.

The stable app/environment project identity reuses the named volume across releases. Redis's append-only setting stores its data there. Docker's [volume reference](https://docs.docker.com/reference/compose-file/volumes/) explains named-volume declarations; Tako additionally scopes resources to the app and environment and rejects external resources, custom resource names, and anonymous volumes in production.

| Check | What it establishes |
| --- | --- |
| `docker info` as `tako` | Service-account access to the server's Docker Engine |
| Successful `/health` | The web process can reach Redis |
| Increasing `visits` | Public requests exercise the application and database |
| Counter continues after a code update | Named-volume data survives the release change |

This is a single-VPS deployment with one selected HTTP ingress service. Keep build inputs and file references within the application directory; writable bind mounts are unsupported in production. If your stack uses secrets, reference Tako secrets explicitly for Compose interpolation. Production does not load an implicit `.env` file. The [Compose reference](/docs/docker-compose/) documents these boundaries.

Named volumes survive stop/start, rollback, and application deletion, but Tako's app-data backups do not include them. Back up Redis separately and verify restoration into a separate stack. Keeping a volume is persistence; recovering a lost VPS requires a backup.

Once the first deploy works, continue with [Compose updates and persistent volumes](/blog/docker-compose-updates-with-persistent-volumes/) or [scaling individual Compose services](/blog/scale-docker-compose-services-on-a-vps/). Start with the counter test: one real write before and after a release tells you more than a green deploy message alone.
