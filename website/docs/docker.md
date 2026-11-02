---
title: Container Releases
description: "Deploy Dockerfile-based applications with Tako's routing, TLS, secrets, health checks, and rolling updates using server-side Podman image builds."
---

For multiple services, see [Docker Compose](/docker-compose/).

Set `container = "Dockerfile"` to deploy an app as an image. Tako uploads the source context and builds the image on each target server with Podman. You write a Dockerfile; a local Docker daemon or image registry is not required for `tako deploy`.

## Configure a container release

```toml tako.toml
name = "container-app"
container = "Dockerfile"

[envs.production]
route = "container.example.com"
servers = ["prod"]
```

Choose an existing [server](/servers/), configure [Secrets](/secrets/) and bindings, and use a [Dockerfile](/dockerfile/) for your application's language. The image owns the production build and command. Container releases reject `main`, `start`, `assets`, `[build]`, and `[[build_stages]]` in `tako.toml`.

```bash
tako deploy --env production
tako status --json
tako logs --env production --days 1 --json
```

Tako builds with Podman, starts the image's default command, probes the app, and rolls healthy replacements into traffic. The server publishes a private loopback port to container port `3000`; Tako's proxy handles public TLS and routing. See [Deployment](/deployment/#rolling-updates) for rollout behavior.

## Runtime contract

| Surface                       | HTTP container                                                   |
| ----------------------------- | ---------------------------------------------------------------- |
| Listener                      | `HOST=0.0.0.0`, `PORT=3000`; use the language SDK.               |
| Secrets and storage bootstrap | `TAKO_BOOTSTRAP_DATA`, read by the SDK.                          |
| Health checks                 | SDK-authenticated internal `/status`.                            |
| Variables and logs            | App environment variables, `TAKO_BUILD`, captured stdout/stderr. |
| Native fd 3 / fd 4            | Not passed into HTTP containers.                                 |
| `TAKO_DATA_DIR`               | Persistent app directory mounted at `/tako/data` by default.      |
| Internal workflow socket      | Not mounted into HTTP containers in v0.                          |

Using an SDK preserves the runtime contract; binding port 3000 alone is insufficient. Rust/Axum and Go apps use their SDK servers. JavaScript fetch apps start through the packaged SDK entrypoint shown in [Dockerfiles](/dockerfile/).

HTTP containers receive the app data directory, including across replacement and rollback. The existing app-data backup includes this directory; SQLite files use the SQLite snapshot path. Each server has its own data, and instances on that server share the app directory. App deletion removes this data. Workflow RPC calls from the HTTP process need the internal socket and therefore cannot be assumed to work in this v0 container path. Separate [container workflow workers](/docker-workflows/) receive the socket.

## Existing images and runtime options

Use `image` instead of `container` to pull an existing image. For an application without a Tako SDK:

```toml tako.toml
name = "existing-app"
image = "ghcr.io/your-org/your-app:1.0"

[container_options]
sdk = false
port = 8080
health_path = "/health"
data_dir = "/data"
secret_env = ["DATABASE_URL"]
# For an image running as a non-root numeric user:
user = { uid = 1000, gid = 1000 }

[envs.production]
route = "app.example.com"
servers = ["prod"]
```

The image must listen on `0.0.0.0` at the configured port. SDK mode defaults to `true`, port to `3000`, health path to `/status`, and data path to `/tako/data`. Plain mode accepts HTTP 2xx or 3xx from the configured path without an internal token. Set `health_host = "app.example.com"` for applications that restrict allowed Host headers. Only names listed in `secret_env` receive secret environment values; SDK mode continues to read secrets through the bootstrap envelope.

`user` uses Podman's `keep-id` mapping so the container UID/GID can write the service-owned data directory without changing its host ownership. Configure the host's rootless Podman subordinate UID/GID ranges before using this option. Omitting it preserves the image's default user; images with a non-root USER may need the explicit mapping. `command = ["argument", "..."]` overrides the image command, and `entrypoint = "/path/to/program"` overrides its entrypoint.

Pulled images receive a release-specific local tag so rollback uses the prepared image even when the upstream tag changes. Stop and replacement explicitly stop/remove the old container. Persistent files remain in the app data directory.

## Development and verification

`tako dev` runs the local dev command or supported runtime/preset defaults; it does not build or run the Dockerfile. For an image smoke test, build/run it with Docker or Podman as described in [Dockerfiles](/dockerfile/#verify-the-image). A local image smoke test does not simulate Tako's full bootstrap or shared services.

After a deployment, request the real hostname, test an application route, and inspect finite logs. Confirm that the image's SDK handles health checks and private values, and check persistence separately from container lifetime. See [container troubleshooting](/troubleshooting/#container-release-does-not-start).
