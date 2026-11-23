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

Example values are not stored in Git. Set local development values as needed:

```bash
tako -c examples/go/basic/tako.toml secret set API_KEY --env development
tako -c examples/go/basic/tako.toml secret set DATABASE_URL --env development
tako -c examples/go/basic/tako.toml secret set EXAMPLE_SECRET --env development
tako -c examples/go/basic/tako.toml generate
```

For deployment, enroll this machine with a network invitation and put the network
ID in the production environment of `tako.toml`. Set production values locally;
deploy or `secret sync --env production` publishes them to shared encrypted state.
