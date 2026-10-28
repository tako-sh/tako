---
title: Gin
description: "Integrate a Gin router with Tako's Go HTTP SDK while preserving application routes, middleware, local HTTPS, secrets, and readiness checks."
---

A Gin engine implements `http.Handler`. Pass it to `tako.ListenAndServe` so Tako owns the listener and runtime protocol.

```bash
go get tako.sh github.com/gin-gonic/gin
```

```go main.go
package main

import (
    "log"
    "net/http"

    "github.com/gin-gonic/gin"
    "tako.sh"
)

func main() {
    app := gin.Default()
    app.GET("/", func(c *gin.Context) {
        c.JSON(http.StatusOK, gin.H{"ok": true})
    })
    if err := tako.ListenAndServe(app); err != nil {
        log.Fatal(err)
    }
}
```

Keep your Gin middleware and route registration. Replace the final `app.Run(...)` call with the SDK listener; do not start both. Tako wraps authenticated internal status handling before dispatching ordinary requests to Gin.

Use the [Go configuration and build setup](/go-http/#configure-and-verify), with `runtime = "go"`. Gin does not need a framework-specific preset. Set secrets through Tako and use generated Go accessors rather than assuming they arrive as individual process environment variables.

## Verify

Run `go test ./...`, then `tako dev --background --json`. Request the returned HTTPS hostname and exercise application middleware, JSON routes, and a missing route. After deployment, repeat the same requests through the public hostname and inspect finite logs.

Source example: [examples/go/gin](https://github.com/tako-sh/tako/tree/master/examples/go/gin). Upstream: [Gin](https://gin-gonic.com/en/docs/).
