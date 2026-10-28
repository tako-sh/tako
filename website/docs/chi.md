---
title: Chi
description: "Connect a Chi v5 router to Tako's Go HTTP SDK, retain standard middleware and subrouters, and verify local HTTPS and deployed route behavior."
---

Chi routers implement `http.Handler`. Keep routing and middleware in Chi and let the Tako SDK own the server lifecycle.

```bash
go get tako.sh github.com/go-chi/chi/v5
```

```go main.go
package main

import (
    "fmt"
    "log"
    "net/http"

    "github.com/go-chi/chi/v5"
    "tako.sh"
)

func main() {
    app := chi.NewRouter()
    app.Get("/", func(w http.ResponseWriter, r *http.Request) {
        fmt.Fprintln(w, "Hello from Chi")
    })
    if err := tako.ListenAndServe(app); err != nil {
        log.Fatal(err)
    }
}
```

Use `tako.ListenAndServe(app)` in place of `http.ListenAndServe(...)`. Your Chi middleware and mounted subrouters remain ordinary Go handlers. Tako handles its authenticated internal status path before application dispatch.

Use `runtime = "go"` and the [shared Go configuration](/go-http/#configure-and-verify). No Chi preset or custom production port is needed. Follow [Secrets](/secrets/) for private values and [Docker](/docker/) when building the application inside an image.

## Verify

Run `go test ./...`, start `tako dev --background --json`, and request the returned HTTPS hostname. Exercise mounted routes, middleware, and a missing path. After deployment, check the public hostname and inspect finite logs.

Source example: [examples/go/chi](https://github.com/tako-sh/tako/tree/master/examples/go/chi). Upstream: [Chi](https://github.com/go-chi/chi).
