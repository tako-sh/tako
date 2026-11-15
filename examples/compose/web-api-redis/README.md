# Compose application group

`shop` is an application group. `web` and `api` have `x-tako`, so they are Tako apps with separate routes and replicas. `redis` has no `x-tako` and remains a Docker service. The group owns deployment, rollback, shared networks, volumes, secrets, and the target server.

The image declares `EXPOSE 3000`. The optional `ports` mappings also let this example run with ordinary Docker Compose. Tako infers the container port and replaces all host publications with private loopback ports. Health checks use standard Compose `healthcheck`; there is no Tako-specific port or health path.

## Local development

From this directory, with Docker and the Compose plugin available:

```bash
tako dev
```

Open `https://shop.tako` and click **Call the API**. The browser calls `https://api.shop.tako/visits`. Development runs one replica of each Tako app. Without explicit development routes, Tako generates `<service>.<group>.tako` (or the configured local development suffix), including for tagged apps without public production routes.

You can also run `docker compose up --build --wait` and open `http://localhost:8080`. Stop that separate project with `docker compose down`; its named volume remains.

## Production

Enroll/configure a target server named `prod`, point `example.com` and `api.example.com` at it, then replace those domains in `compose.yaml` and `tako.toml` with your own:

```bash
tako deploy --env production
tako status --json
tako scale 3 --service api --env production
```

Each app starts with one replica. Scaling it leaves the frontend and Redis at their own counts. With multiple Tako apps, `tako scale` requires `--service`.

Stop and resume the API independently:

```bash
tako stop --service api --env production
tako status --json
tako start --service api --env production
```

While the API is stopped, the frontend and Redis keep running, but API requests are unavailable. Status still lists the API with `state: "stopped"`. Starting it restores its configured count (three after the scale command above). Stop intent and replica overrides survive redeploys, rollbacks, and server restarts; scaling a stopped API changes its next start's count without resuming it.

Omit `--service` to stop or start the entire group. Group start preserves any individually stopped apps. Ordinary Docker services such as Redis have no individual start/stop command. A service cannot be deleted independently: edit `compose.yaml` and redeploy to remove it, or use `tako delete --env production` to delete the group. Named volumes remain.

Call `/visits`, redeploy, and call it again to verify persistence. Deployments and rollbacks keep the Docker volume; backing up Tako app data does not back up Redis's Docker volume.

`x-tako.envs.<environment>` accepts `route` or `routes`, plus optional `source_ip` (inheriting the group's setting). Settings for deployment targets, TLS credentials/provider, storage bindings and backups remain group settings in `tako.toml`; container commands, environment, health checks, resources and replica defaults remain Compose settings. Unsupported `x-tako` fields fail validation. A tagged app with no route still has its own private backend; omitting `x-tako` keeps a service entirely under Compose.
