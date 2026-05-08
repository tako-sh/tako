# Go Examples

- `basic/` — net/http
- `gin/` — [Gin](https://github.com/gin-gonic/gin)
- `echo/` — [Echo](https://echo.labstack.com/)
- `chi/` — [Chi](https://github.com/go-chi/chi)

## Run

```bash
just tako examples/go/basic dev
```

Or directly:

```bash
cd examples/go/basic
go run .
```

## Secrets

Each example has pre-configured encrypted development and production secrets.
Passphrase: `tako-example`

```bash
printf '%s\n' 'tako-example' | \
  tako -c examples/go/basic/tako.toml secrets key import --passphrase --env development
tako -c examples/go/basic/tako.toml secrets list
```

Import the `production` key the same way before deploying an example.
