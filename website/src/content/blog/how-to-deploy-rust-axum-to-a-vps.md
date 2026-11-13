---
title: "How to Deploy a Rust Axum App to a VPS with Automatic HTTPS"
date: "2026-11-02T03:15"
description: "Deploy a Rust Axum app to a VPS with Tako's SDK, a multi-stage Dockerfile, automatic HTTPS, health checks, and rolling updates."
image: 58ad61cea9a8
imageAlt: "A coral octopus secures an Axum application module inside a VPS server as an HTTPS lock lights up."
---

> **Update, November 13, 2026:** This article describes an earlier Tako release. Server commands now use `tako network server`; native network membership supplies the inventory. See the [current enrollment instructions](/docs/networks/).

> **Networking update.** The Tailscale and NetBird integrations and older setup commands described here have been retired. For new installations, see [native WireGuard networks](/blog/native-wireguard-networks-for-vps-apps/). For an existing Tailscale or NetBird host, follow the [migration guide](/blog/migrate-tailscale-to-tako-wireguard/).

Your Axum router works locally. Getting it onto a VPS adds a second project: build a Linux binary, keep it running, issue a certificate, and work out whether the next version is healthy before sending users to it.

We'll take a small Rust app through that path with Tako. The app uses the Rust SDK, a Dockerfile supplies the production build, and Tako handles public HTTPS and rolling releases. You keep your router. The certificate has somewhere else to live.

## Build an Axum app with the Tako Rust SDK

Start with a Rust toolchain and create a binary project:

```bash
cargo new axum-on-tako --bin
cd axum-on-tako
```

Keep the package metadata Cargo created and add these dependencies to `Cargo.toml`:

```toml
[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tako = { version = "0.2.1", features = ["axum"] }
```

The `tako` crate's `axum` feature provides the serving adapter. This example follows the [Rust and Axum guide](/docs/axum/); the implementation is in the [Rust SDK source](https://github.com/tako-sh/tako/tree/master/sdk/rust).

Replace `src/main.rs` with:

```rust
use axum::{routing::get, Router};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(|| async { "Hello from Axum on Tako" }))
        .route("/api/health", get(|| async { "ok" }));

    tako::axum::serve(app).await
}
```

The routes use Axum's normal [`Router` and `get` APIs](https://github.com/tokio-rs/axum). The serving call is the integration point: `tako::axum::serve` binds the SDK listener and wraps your router with authenticated internal status handling. Keep your existing handlers and middleware on that router.

Our `/api/health` endpoint is an application smoke test. Tako's internal `/status` probe has its own host and token checks, supplied by the SDK. You don't need to turn the public endpoint into an imitation of that protocol.

Generate the lockfile and check the app:

```bash
cargo generate-lockfile
cargo check
cargo test
```

Keep `Cargo.lock` with the application source: our production build will use `--locked`. We'll exercise the HTTP routes through the image and Tako-managed local development below. If you're adapting an existing workspace, substitute its binary name wherever this guide uses `axum-on-tako`.

The SDK also exposes runtime secrets through `tako::secret("NAME")`. We'll leave this app secret-free so the first deploy only needs to prove routing and startup. When adding a database or API credentials, follow the [Secrets guide](/docs/secrets/) rather than copying values into the Dockerfile.

## Package the Rust binary in a container release

Create a `Dockerfile` in the project root:

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release --bin axum-on-tako

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/axum-on-tako /app/app
ENV HOST=0.0.0.0 PORT=3000
EXPOSE 3000
CMD ["/app/app"]
```

This follows Tako's [Rust Dockerfile example](/docs/dockerfile/). The first stage compiles the application; the second copies the binary into the runtime image. Docker's [multi-stage build guide](https://docs.docker.com/build/building/multi-stage/) explains the same separation between build tools and shipped artifacts.

Both stages use Bookworm here to keep their native-library baseline aligned. Projects that link additional system libraries must provide compatible runtime packages too. Pin the tested base-image versions or digests for production; the broad tags above keep the example readable.

Add a `.dockerignore`:

```text
.git
.tako
.env*
target
```

Tako first stages the upload context respecting `.gitignore` and its own exclusions. The image builder then applies `.dockerignore`. Ensure your Rust sources, manifests, lockfile, and Dockerfile survive both steps. A missing source file cannot be recovered by `COPY . .` after it has already been excluded from the upload.

If Docker is available locally, smoke-test the image:

```bash
docker build -t axum-on-tako .
docker run --rm -p 3000:3000 axum-on-tako
```

In another terminal, request `http://localhost:3000/` and `http://localhost:3000/api/health`; expect the greeting and `ok`. This verifies image assembly and the HTTP listener; the full Tako bootstrap and deployment checks happen on the server.

You can use Podman for that local smoke test instead. A local image builder is optional for `tako deploy`: Tako uploads the source and builds the image on the target VPS with Podman. This gives the Rust compiler a Linux build environment without requiring a separately published application image.

Create `tako.toml`:

```toml
name = "axum-on-tako"
container = "Dockerfile"
dev = "cargo run"

[envs.production]
route = "axum.example.com"
servers = ["prod"]
```

The Dockerfile owns the production build and startup command. Container releases reject `main`, `start`, `assets`, `[build]`, and `[[build_stages]]` in this configuration. There is no built-in Rust preset or `runtime = "rust"`; this tutorial uses the documented [container release path](/docs/docker/).

The explicit `dev` command runs Rust locally. After installing the CLI in the next section, run [`tako dev`](/docs/development/) from this configured project. Open the HTTPS URL it reports and request `/api/health` there too. Tako supplies the SDK bootstrap and listener settings for that process. Local development does not build or run the Dockerfile, so check the image separately when changing system packages or image assembly.

## Deploy to the VPS and verify HTTPS

Install the Tako CLI on your workstation:

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

Use a supported Linux VPS with glibc, systemd, and Tako's required isolation controllers. Connect it and your workstation through Tailscale or NetBird, allowing SSH and private management traffic on TCP `9844`. The [server setup guide](/docs/servers/) covers the requirements and access policies.

For an existing NetBird network, enroll the server like this:

```bash
tako servers add ubuntu@my-server.netbird.cloud --vpn netbird --name prod
tako servers list --json
```

Replace the hostname and administrator with your actual values. Interactive setup offers installation or repair if needed. If you're using Tailscale, select `--vpn tailscale` and its server hostname instead. The inventory name `prod` must match the name in `tako.toml`.

Point `axum.example.com` at the VPS's public address. Use your own domain, and allow public HTTP `80` and HTTPS `443`. For this direct-to-VPS setup, the default Let's Encrypt mode uses HTTP-01 validation. A route declaration does not create public DNS records; the [TLS guide](/docs/routing/) explains the certificate choices.

Deploy from the project directory:

```bash
tako deploy --env production
```

The server builds the image, starts its default command, and checks the SDK's internal status response before routing public requests to healthy replacement instances. The path looks like this:

```d2
direction: right
laptop: "Laptop\nRust source + Dockerfile"
vps: "VPS" {
  build: "Podman image build"
  proxy: "Tako proxy\npublic HTTPS" {style.fill: "#E88783"}
  app: "Axum + Tako SDK\ncontainer port 3000" {style.fill: "#9BC4B6"}
  build -> app
  proxy -> app: "private loopback mapping"
}
browser: "Browser"
laptop -> vps.build: "source upload"
browser -> vps.proxy: "axum.example.com"
```

Check the actual deployment:

```bash
tako status --json
tako logs --env production --days 1 --json
curl -i https://axum.example.com/
curl -i https://axum.example.com/api/health
```

Confirm the certificate, greeting, and `ok` response. A healthy rollout checks startup; exercising application endpoints checks the behavior users receive. Repeat the public requests after the next deploy, especially when changing routes, dependencies, or bootstrap behavior.

Keep these container boundaries in mind as the app grows:

| Concern | This deployment |
| --- | --- |
| HTTP listener | SDK binds `0.0.0.0:3000` inside the container. |
| Public HTTPS | Tako terminates TLS and routes traffic to healthy instances. |
| Private values | SDK reads container bootstrap from `TAKO_BOOTSTRAP_DATA`. |
| Persistent files | Tako mounts the app data directory at `/tako/data` by default and exposes it through `TAKO_DATA_DIR`; instances on one server share it. |

The [deployment guide](/docs/deployment/) covers rolling updates, and [Operations](/docs/operations/) covers logs and release inspection. Start with these two routes, verify one real deploy, then bring the rest of your Axum application along with the same SDK serving call and image build.
