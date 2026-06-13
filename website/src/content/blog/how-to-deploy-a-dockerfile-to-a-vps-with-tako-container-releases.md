---
title: "How to Deploy a Dockerfile to a VPS with Tako Container Releases"
date: "2026-06-13T08:08"
description: 'Use container = "Dockerfile" to ship Dockerfile-shaped apps to a VPS while Tako keeps routing, TLS, secrets, logs, and rolling updates.'
image: 5530a4adbe14
---

Tako still does not default to Docker. Native releases are faster, smaller, and simpler for the Bun, Node, and Go apps Tako understands directly.

But sometimes your app is already shaped like a container. Maybe it needs system packages. Maybe the runtime is not a native Tako runtime yet. Maybe your team already has a carefully tuned Dockerfile and you just want the VPS deploy experience around it.

That is what container releases are for. Set `container = "Dockerfile"` in `tako.toml`, and [`tako deploy`](/docs/deployment/) packages your source, uploads it to your server, builds the image there with Podman, and runs it behind the same Tako routing, TLS, secrets, logs, and rolling update machinery as the rest of your apps.

## Start With A Tako-Shaped App

The app inside the container still needs to speak the Tako runtime contract. That is what the SDK is for: health checks, bootstrap data, graceful shutdown, secrets, and storage bindings.

Here is a small Go app:

```go
package main

import (
	"fmt"
	"net/http"
	"os"

	"tako.sh"
)

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "Hello from a Tako container release")
	})

	if err := tako.ListenAndServe(mux); err != nil {
		fmt.Fprintf(os.Stderr, "server error: %v\n", err)
		os.Exit(1)
	}
}
```

`tako.ListenAndServe` reads the container environment, binds to `$HOST:$PORT`, serves the built-in internal `/status` endpoint, and reads secrets from the bootstrap envelope. For container releases, that envelope arrives in `TAKO_BOOTSTRAP_DATA` instead of fd 3, because the native file-descriptor bootstrap does not cross the container boundary in v0.

Now add a Dockerfile:

```dockerfile
# syntax=docker/dockerfile:1

FROM golang:1.24-alpine AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /app .

FROM alpine:3.21
COPY --from=build /app /app
EXPOSE 3000
CMD ["/app"]
```

Docker's [Dockerfile reference](https://docs.docker.com/reference/dockerfile/) treats `EXPOSE` as documentation for the port an image expects to serve. Tako's runtime contract is more specific: the HTTP container gets `HOST=0.0.0.0` and `PORT=3000`, and Tako publishes a server-assigned loopback port to that container port. Your app should listen on the env values the SDK provides, not hard-code a public port.

Add a `.dockerignore` too. The container file and `.dockerignore` own production build inputs for container releases:

```text
.git
.tako
.env*
node_modules
tmp
dist
```

Docker's [.dockerignore guide](https://docs.docker.com/build/building/context/#dockerignore-files) is still the right mental model here: keep local junk, secrets, and build leftovers out of the image context.

## Configure The Container Release

Here is the `tako.toml`:

```toml
name = "container-demo"
runtime = "go"
container = "Dockerfile"
dev = ["go", "run", "."]

[envs.production]
route = "container.example.com"
servers = ["prod"]
```

That one `container` line changes the production packaging path. Native packaging fields are no longer used for deploys, so this is intentionally invalid:

```toml
container = "Dockerfile"
main = "app"

[build]
run = "go build -o app ."
```

For a container release, the Dockerfile owns the production build. Tako will reject `main`, `start`, `assets`, `[build]`, and `[[build_stages]]` alongside `container` so there is only one build contract to reason about. The [`tako.toml` reference](/docs/tako-toml/) has the full schema and the current container-release limits.

Local development stays boring on purpose:

```bash
tako dev
```

`tako dev` does not build or run your Dockerfile locally. It uses `dev`, the preset dev command, or the native runtime default. In the config above, local dev is just `go run .` behind Tako's local HTTPS proxy. The container path is for production deploys.

Register the VPS once:

```bash
tako servers add 203.0.113.10 --name prod
```

`tako servers add` installs the server side pieces, including Podman for container releases. Server upgrades also install Podman when it is missing. The day-two commands live in the [CLI reference](/docs/cli/), but for a first deploy this is the important bit: the server named `prod` is now a deploy target.

Then ship it:

```bash
tako deploy
```

Here is the flow:

```d2
direction: right

laptop: "Laptop\nsource + tako.toml" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
archive: "Source archive\nDockerfile + .dockerignore" {style.fill: "#9BC4B6"; style.font-size: 16}
server: "VPS\ntako-server" {style.fill: "#E88783"; style.font-size: 16}
image: "Podman build\ntako/app-env:version" {style.fill: "#9BC4B6"; style.font-size: 16}
instance: "HTTP container\nHOST=0.0.0.0 PORT=3000" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
proxy: "Pingora proxy\nTLS + routing" {style.fill: "#E88783"; style.font-size: 16}
internet: "https://container.example.com" {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}

laptop -> archive: "tako deploy"
archive -> server: "upload"
server -> image: "build"
image -> instance: "start + probe"
instance -> proxy: "healthy"
proxy -> internet: "serve"
```

On the server, `tako-server` builds the image from the uploaded app directory, tags it as `tako/{app}-{env}:{version}`, starts HTTP containers from the image's Dockerfile defaults, and publishes each one on `127.0.0.1:<assigned-port>:3000`. Pingora terminates HTTPS and routes public traffic only to healthy instances.

That means a Dockerfile-shaped app still gets the usual Tako deploy behavior:

| Concern        | What Tako keeps doing                                                          |
| -------------- | ------------------------------------------------------------------------------ |
| Public routing | Routes `container.example.com` to healthy instances behind Pingora             |
| TLS            | Issues and renews certificates according to the environment's SSL mode         |
| Secrets        | Stores secrets encrypted and injects fresh bootstrap data into new containers  |
| Logs           | Streams app and proxy logs through `tako logs --env production --tail`         |
| Updates        | Starts the new release, probes `/status`, drains old instances, then finalizes |

## The Rules That Matter

Container releases are deliberately small in v0. The shape is easy to remember:

| Rule                         | Why it exists                                                                   |
| ---------------------------- | ------------------------------------------------------------------------------- |
| `container` path is relative | The container file must stay inside the app directory                           |
| Dockerfile owns production   | No `main`, `start`, `assets`, `[build]`, or `[[build_stages]]` at the same time |
| Container listens on `3000`  | Tako maps an internal loopback port to container port `3000`                    |
| Use the Tako SDK             | Health checks, secrets, storages, and internal status depend on the SDK         |
| Secrets are not env vars     | They arrive inside `TAKO_BOOTSTRAP_DATA`, then the SDK exposes them safely      |
| `TAKO_DATA_DIR` is not set   | Persistent app data is not mounted into HTTP containers in v0                   |

That secrets point is easy to miss. Do this:

```bash
tako secrets set DATABASE_URL
tako deploy
```

Then read the secret through the SDK, not `os.Getenv("DATABASE_URL")`. In Go, `tako generate` creates typed helpers in `tako_secrets.go`; in JavaScript, `tako.secrets.DATABASE_URL` is the server-side runtime API. The same [secrets model](/blog/secrets-without-env-files/) works for native and container releases, but containers receive it through the environment bootstrap envelope instead of fd 3.

Workflow workers can also run from a container image, with one current limit: configure one workflow `run` command, and Tako starts a separate container from the same image with that command as the entrypoint. HTTP containers do not receive the internal socket in v0; workflow containers do, because they need it to talk to the workflow engine. The [deployment guide](/docs/deployment/) tracks those details as the protocol evolves.

## Why This Exists

We wrote [why Tako does not default to Docker](/blog/why-we-dont-default-to-docker/) because the native path is still the happy path for most apps. If you can run as a direct Bun, Node, or Go process, you should. It keeps deploys fast and the moving parts small.

`container = "Dockerfile"` is the escape hatch for everything else. You keep your Dockerfile, your system packages, and your image build. Tako keeps the platform layer around it: VPS registration, HTTPS, routing, secrets, logs, health checks, and rolling updates.

That is the point of Tako's v0 protocol. The deploy artifact can be native or container-shaped. The thing around it stays the same.
