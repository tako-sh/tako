---
title: "Caddy vs Traefik vs Nginx for VPS App Deployment"
date: "2026-05-21T13:42"
description: "Compare Caddy, Traefik, and Nginx for VPS app deployment, then see why Tako folds proxying, TLS, deploys, and process state together."
image: d9a40f150813
---

Choosing a reverse proxy for a VPS used to be most of the deployment conversation: put Nginx in front of your app, point a domain at the box, wire up certificates, and call it a day. Then Caddy made HTTPS feel automatic. Then Traefik made Docker and Kubernetes routing feel automatic. All three are good tools, and if all you need is a reverse proxy, one of them is probably the right answer.

But "deploy an app to a VPS" is bigger than proxying port `3000` to `example.com`. You also need to decide who owns process startup, rolling updates, health checks, changing ports, certificate renewal, static assets, logs, secrets, and the moment a cold app needs to wake up.

That is the gap Tako is built around. This is not another "which proxy is fastest?" post. We already covered why Tako uses Pingora in [Pingora vs Caddy vs Traefik](/blog/pingora-vs-caddy-vs-traefik). This is about deployment shape: what Caddy, Traefik, and Nginx give you on a VPS, and why Tako folds the proxy into the same control plane as deploys and app processes.

## The short version

| Tool                                                                                          | Best fit                                               | Config source                         | TLS story                                                            | Deployment gap                                                |
| --------------------------------------------------------------------------------------------- | ------------------------------------------------------ | ------------------------------------- | -------------------------------------------------------------------- | ------------------------------------------------------------- |
| [Caddy](https://caddyserver.com/docs/automatic-https)                                         | Simple VPS reverse proxy with easy HTTPS               | Caddyfile, JSON API, CLI              | Automatic HTTPS by default for qualifying hostnames                  | App process lifecycle still lives somewhere else              |
| [Traefik](https://doc.traefik.io/traefik/reference/install-configuration/providers/overview/) | Docker, Swarm, Kubernetes, and provider-driven routing | Providers, labels, annotations, files | ACME via certificate resolvers, referenced by routers or entrypoints | Best when infrastructure already exposes service metadata     |
| [Nginx](https://docs.nginx.com/nginx/admin-guide/web-server/reverse-proxy)                    | Explicit, battle-tested reverse proxy config           | `nginx.conf` and included files       | Usually paired with separate certificate tooling                     | Deploy orchestration and reload safety are your scripts       |
| Tako                                                                                          | App deployment on your own server                      | `tako.toml` plus server state         | Built into `tako-server` per deployed route                          | Proxy, deploy, health, TLS, and process state share one model |

Tako is for a different question: "What if the deployment tool owned the proxy too?"

## Caddy: the easiest HTTPS path

Caddy's superpower is right there in the docs: [automatic HTTPS](https://caddyserver.com/docs/automatic-https). Give Caddy a qualifying hostname, point DNS at the server, make ports `80` and `443` reachable, and it can obtain certificates, renew them, and redirect HTTP to HTTPS.

For a single app with a stable port, that is hard to beat. Caddy's [`reverse_proxy`](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy) directive also covers the things you expect from a real proxy: multiple upstreams, load balancing, health checks, retries, WebSockets, header manipulation, and streaming behavior.

The catch is not Caddy. The catch is what sits beside it. Something still needs to start your app, choose its port, update the Caddyfile, reload Caddy when routes change, decide when a new version is healthy, drain the old process, and keep secrets out of loose `.env` files. Caddy makes the proxy layer pleasant; it does not become your app lifecycle manager.

## Traefik: the dynamic infrastructure proxy

Traefik shines when the infrastructure around it already describes services. Its [provider model](https://doc.traefik.io/traefik/reference/install-configuration/providers/overview/) lets Traefik query Docker, Kubernetes, Swarm, Nomad, Consul, ECS, files, and other sources, then update routes dynamically when those sources change.

With Docker, routing can live next to the container through labels. The [Docker provider](https://doc.traefik.io/traefik/reference/routing-configuration/other-providers/docker/) can use those labels to generate routing rules. Traefik's ACME support lives behind [certificate resolvers](https://doc.traefik.io/traefik/reference/install-configuration/tls/certificate-resolvers/overview/), which routers or entrypoints explicitly reference.

That model is powerful when Docker or Kubernetes is already the control plane. The service registry exists. Ports are discoverable. Deploys create or update container metadata. Traefik follows the metadata and updates the edge.

On a plain VPS with native processes, that same strength becomes less useful. A Bun or Node process on `127.0.0.1:41873` is not a Docker container with labels. If you skip containers, you need another system to decide what should be exposed and when. Traefik can still run with file config, but then you are back to maintaining the routing source yourself.

Traefik is excellent at reading infrastructure state. Tako's bet is that, for native-process VPS apps, the deployment tool should be the infrastructure state.

## Nginx: the explicit classic

Nginx remains the known quantity. Its [reverse proxy guide](https://docs.nginx.com/nginx/admin-guide/web-server/reverse-proxy) explains the core model: match a location, `proxy_pass` to an upstream, adjust headers when needed, and let Nginx fetch the response and send it back to the client.

Nginx is a great fit when you want the proxy to be explicit and boring. You write the config, test it, reload it, and know exactly what is supposed to happen. Its [runtime control docs](https://docs.nginx.com/nginx/admin-guide/basic-functionality/runtime-control/) cover the standard reload path, including `HUP`.

The deployment problem is the same one, just more manual. If a new release starts on a new port, something has to update the upstream, health-check the new process, reload Nginx safely, and stop the old process after in-flight requests drain.

Nginx gives you sharp tools. A deployment platform still has to decide how to use them.

## The part proxies do not know

A standalone proxy is responsible for traffic. A deployment platform is responsible for state.

| Question                                            | Standalone proxy answer                     | Deployment-platform answer                                        |
| --------------------------------------------------- | ------------------------------------------- | ----------------------------------------------------------------- |
| Which app owns `example.com/api/*`?                 | Whatever the current config says            | The app environment declares it in [`tako.toml`](/docs/tako-toml) |
| Which port is healthy right now?                    | A configured upstream or discovered service | The process that passed readiness and health checks               |
| Can this request wake a stopped app?                | Usually no, unless another layer does it    | Yes, if the app is scaled to zero                                 |
| When should the old version stop receiving traffic? | During a reload or external upstream switch | During the rolling deploy flow                                    |
| Where do deploy logs and proxy diagnostics meet?    | Usually separate systems                    | One app-scoped log stream                                         |
| Who owns certificate setup for new routes?          | Proxy or external ACME tooling              | The same deploy that registers the route                          |

That separation is manageable for one app. It gets noisy with several apps, staging environments, path-prefixed routes, wildcard domains, and low-traffic tools that should scale to zero. This is why Tako does not treat the proxy as a bolt-on: [`tako deploy`](/docs/deployment) already knows the app name, environment, routes, build version, runtime, release command, secrets, desired scale, and target server. The proxy should be able to ask that state directly.

```d2
direction: right

standalone: "Standalone proxy stack" {
  direction: down
  deploy: "deploy script"
  proc: "process manager"
  proxy: "Caddy / Traefik / Nginx"
  certs: "certificate tool"
  logs: "separate logs"

  deploy -> proc: "start app"
  deploy -> proxy: "write or discover route"
  certs -> proxy: "cert + reload"
  proc -> logs: "stdout"
  proxy -> logs: "access/errors"
}

tako: "Tako" {
  direction: down
  cli: "tako deploy"
  server: "tako-server"
  pingora: "Pingora proxy"
  app: "native app process"

  cli -> server: "artifact + route + env"
  server -> app: "spawn + readiness"
  server -> pingora: "route + healthy backend"
  pingora -> app: "loopback request"
}
```

## Why Tako folds it together

Tako uses Pingora internally, but the user-facing difference is not "another reverse proxy." The difference is that the proxy participates in the app lifecycle.

When you deploy, Tako uploads the artifact, prepares the runtime, runs an optional release command, starts a new instance, waits for SDK readiness, probes health, shifts traffic, and drains the old process. The proxy does not need a regenerated config file; it is reading the same app state the deploy flow just updated.

When you define routes, they live with the app:

```toml
name = "api"
runtime = "bun"

[envs.production]
routes = ["api.example.com", "example.com/api/*"]
servers = ["prod"]
```

Those routes drive TLS, conflict detection, static asset handling, and request matching. If the app is scaled to zero with [`tako scale`](/docs/cli), the next matching request can trigger a cold start and wait for the instance to become healthy. If the app has static files in `public/`, the proxy can serve them directly before waking or forwarding to the process.

That is the whole control-plane argument: proxying, TLS, deploys, and process state are not four unrelated chores. They are four views of the same app.

## Which should you use?

Use Tako when you want the VPS to feel more like a platform: [`tako.toml`](/docs/tako-toml) for routes, [`tako deploy`](/docs/deployment) for releases, built-in TLS, native process management, scale-to-zero, app logs, secrets, and local HTTPS through [`tako dev`](/docs/development).
