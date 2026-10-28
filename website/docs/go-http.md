---
title: Go net/http
description: "Serve a Go http.Handler with Tako's listener, authenticated status endpoint, bootstrap secrets, and native build configuration for deployment."
---

The root Go module is `tako.sh`. Its `ListenAndServe` accepts any `http.Handler`, including the standard library router and [Gin](/gin/), [Echo](/echo/), or [Chi](/chi/) routers.

## Connect the handler

```bash
go get tako.sh
```

```go main.go
package main

import (
    "fmt"
    "log"
    "net/http"

    "tako.sh"
)

func main() {
    mux := http.NewServeMux()
    mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
        fmt.Fprintln(w, "Hello from Tako")
    })
    if err := tako.ListenAndServe(mux); err != nil {
        log.Fatal(err)
    }
}
```

Tako owns the listening address, readiness signal, authenticated internal `/status`, and graceful shutdown. For a custom server loop use `tako.Listener()` and retain internal status handling; see the [Go SDK skill](https://tako.sh/.well-known/agent-skills/tako-sdk-go/SKILL.md).

## Configure and verify

```toml tako.toml
name = "go-app"
runtime = "go"
main = "app"

[build]
run = "CGO_ENABLED=0 go build -o app ."

[envs.production]
route = "go.example.com"
servers = ["prod"]
```

Use the CGO setting appropriate to your dependencies. Build for the selected server's OS/architecture; copying a macOS binary does not produce a Linux release. See [Deployment](/deployment/) for build/artifact behavior and [Docker](/docker/) for server-side image builds.

Run `go test ./...`, start `tako dev --background --json`, and request the returned HTTPS URL. Set private values with [Secrets](/secrets/) and run `tako generate` for typed Go accessors. [Presence](/presence/) and [Stores](/stores/) have Go backend registrations consumed by the shared browser clients.

Source example: [examples/go/basic](https://github.com/tako-sh/tako/tree/master/examples/go/basic).
