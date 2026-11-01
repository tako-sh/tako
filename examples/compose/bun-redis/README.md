# Bun and Redis with Compose

A small Bun HTTP app and Redis. Each request to `/` increments a counter in Redis and returns it as JSON. `/health` checks the Redis connection without changing the counter. The app uses Bun's built-in Redis client and needs no package install or Tako SDK.

## Run locally

With Docker Engine and Compose available, run `tako dev` to build both services and open the app through Tako's local HTTPS hostname. Press `r` to rebuild after edits. Stopping and restarting the dev app preserves its Redis counter; each checkout and dev variant has separate data.

To run directly with Compose instead:

```bash
docker compose up --build --wait
curl http://localhost:8080/
curl http://localhost:8080/
```

The responses show increasing `visits` values. Redis uses append-only persistence in the `redis-data` named volume. Run `docker compose down`, then `docker compose up --wait` and request `/` again to confirm the counter survives replacement. Adding `--volumes` to `down` deletes the data.

Only the Bun app publishes a port. Redis is accessible to the app through the Compose network.

## Deploy with Tako

Change the domain and server in `tako.toml`. The server needs Docker Engine and Docker Compose 2.24.4 or newer accessible to the Tako service user.

```bash
tako deploy --env production
tako status --json
```

Tako replaces the local port mapping with private ingress behind HTTPS. A Bun-only update starts a healthy replacement, switches traffic, and drains the old container while unchanged Redis keeps running. Changing Redis itself uses stop-first replacement and can cause downtime. Request your domain before and after an update to verify the counter persists. Tako app-data backups do not include Docker named volumes; back up Redis separately when using this pattern for real data.
