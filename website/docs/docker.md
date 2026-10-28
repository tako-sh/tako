---
title: Container Releases
description: "Deploy Dockerfile-based applications with Tako's routing, TLS, secrets, health checks, and rolling updates using server-side Podman image builds."
---

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
| `TAKO_DATA_DIR`               | Not provided or mounted into HTTP containers in v0.              |
| Internal workflow socket      | Not mounted into HTTP containers in v0.                          |

Using an SDK preserves the runtime contract; binding port 3000 alone is insufficient. Rust/Axum and Go apps use their SDK servers. JavaScript fetch apps start through the packaged SDK entrypoint shown in [Dockerfiles](/dockerfile/).

Use external databases and suitable object storage for persistent container data. `tako.cache` requires a data directory and is not available by default in HTTP containers. Workflow RPC calls from the HTTP process need the internal socket and therefore cannot be assumed to work in this v0 container path. Separate [container workflow workers](/docker-workflows/) receive the socket.

## Development and verification

`tako dev` runs the local dev command or supported runtime/preset defaults; it does not build or run the Dockerfile. For an image smoke test, build/run it with Docker or Podman as described in [Dockerfiles](/dockerfile/#verify-the-image). A local image smoke test does not simulate Tako's full bootstrap or shared services.

After a deployment, request the real hostname, test an application route, and inspect finite logs. Confirm that the image's SDK handles health checks and private values, and check persistence separately from container lifetime. See [container troubleshooting](/troubleshooting/#container-release-does-not-start).
