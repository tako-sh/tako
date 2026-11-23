# Compose SDK fixture

`web` and `worker` are `sdk: true` services from one image; `redis` and `nginx`
are plain services. The E2E runner deploys the group and checks secrets,
channels published from the worker and received through the web route,
workflows enqueued by `web` and run by `worker`, a cron schedule, `tako.dataDir`
across deploys and a server restart, that a secret rotation replaces only the
SDK services, and rollback.

Both server deployments use `[envs.production.channels].database = "channel-replay"`.
The full client creates that database once on Ubuntu before deploying; the
AlmaLinux SDK services publish through their local broker to the same home.
