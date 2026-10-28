---
title: Echo
description: "Run an Echo v5 application through Tako's Go SDK with the existing router and middleware, authenticated status handling, and managed local HTTPS."
---

Echo implements `http.Handler`, so Tako can host it without Echo opening its own listener. This example uses Echo v5, matching the repository example; v4 applications retain their v4 import and context types.

```bash
go get tako.sh github.com/labstack/echo/v5
```

```go main.go
package main

import (
    "log"
    "net/http"

    "github.com/labstack/echo/v5"
    "tako.sh"
)

func main() {
    app := echo.New()
    app.GET("/", func(c *echo.Context) error {
        return c.JSON(http.StatusOK, map[string]bool{"ok": true})
    })
    if err := tako.ListenAndServe(app); err != nil {
        log.Fatal(err)
    }
}
```

Preserve route registration, middleware, and error handlers. Replace Echo's final server-start call with `tako.ListenAndServe`; Tako provides readiness, internal status authentication, and graceful shutdown. Use `runtime = "go"` and the [Go build configuration](/go-http/#configure-and-verify). No Echo-specific preset is required.

Use [Secrets](/secrets/) for private application values. [Presence](/presence/) and [Stores](/stores/) registrations work through the Go HTTP SDK independently of Echo routing.

## Verify

Run `go test ./...`, start with `tako dev --background --json`, and request the returned HTTPS URL. Test middleware and error responses as well as the happy path. Deploy through [Deployment](/deployment/) and verify the public route and finite logs.

Source example: [examples/go/echo](https://github.com/tako-sh/tako/tree/master/examples/go/echo). Upstream: [Echo](https://echo.labstack.com/).
