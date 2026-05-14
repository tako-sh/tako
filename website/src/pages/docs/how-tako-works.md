---
layout: ../../layouts/DocsLayout.astro
title: "How Tako works: rolling deploys, TLS, health checks, and scale to zero - Tako Docs"
heading: "How Tako Works"
current: how-tako-works
description: "Learn how Tako handles local development, rolling deploys, TLS, health checks, request routing, scaling, and runtime management."
---

# How Tako Works

Tako pairs a local CLI with a self-hosted server runtime. The CLI owns project configuration, local development, builds, deploy orchestration, secrets, and server management. `tako-server` runs on your hosts and owns routing, TLS, app and worker processes, health checks, scale-to-zero, channels, workflows, image optimization, and rolling updates.

The protocol is v0. Runtime behavior lives in runtime plugins, while presets only provide framework metadata such as entrypoints, assets, and dev commands.

## Main Pieces

### `tako` CLI

The CLI reads `./tako.toml` by default. Pass `-c` or `--config <CONFIG>` to select another config file; the selected file's parent directory is the app directory.

The CLI can:

- create project config with `tako init`
- run trusted local HTTPS with `tako dev`
- build and deploy with `tako deploy`
- manage encrypted local secrets
- attach encrypted object storage bindings
- register, upgrade, and inspect servers
- read logs, releases, status, and scale settings

Most remote management reads and mutations use signed HTTP over the server's private Tailscale address. SSH is still used for setup, recovery, upload, and direct host maintenance.

### `tako-server`

`tako-server` is the production runtime installed on each host. It listens on public HTTP and HTTPS ports, routes requests to the right app, manages certificates, supervises app instances, stores runtime state in SQLite, and exposes a private management API.

Normal host bootstraps use `/opt/tako` as the server data directory and `/var/run/tako/tako.sock` as the local management socket. The installer lays down the service but leaves it stopped; `tako servers add` configures private management and starts the service.

### SDKs

Tako apps use SDKs to speak the runtime protocol:

- JavaScript and TypeScript apps export a Web Standard fetch handler and run through SDK entrypoints for Bun or Node.
- Go apps call `tako.ListenAndServe()` or `tako.Listener()` and compile to a native binary.

The SDK writes its bound loopback port to fd 4 when ready. It reads internal auth, secrets, and storage bindings from fd 3 before user code runs.

## App Identity

Each deployed app is identified as `{app}/{env}` on a server. The app name comes from top-level `name` in `tako.toml`; if omitted, Tako derives it from the selected config file's parent directory.

Set `name` for long-lived apps. Changing it later creates a new server-side identity and path.

Names must start with a lowercase letter, use only lowercase letters, numbers, and hyphens, end with a letter or number, and be at most 63 characters.

## Local Development

`tako dev` is a client for a persistent local daemon. It prepares local DNS, TLS, and proxy prerequisites, starts `tako-dev-server` if needed, registers the selected config file, starts the app process, and streams logs into the terminal.

Default local route:

```text
https://{app}.test/
```

Tako also keeps `.tako.test` available as a fallback zone. Managed local DNS applies only to `.test` and `.tako.test` routes. External development hostnames can be routed through the proxy, but you point those DNS records at Tako yourself.

On macOS, Tako uses a launchd-managed loopback proxy on `127.77.0.1:80` and `127.77.0.1:443`. On Linux, Tako uses the same loopback address with iptables and systemd-resolved setup. Both paths use a local CA so browsers trust app certificates after the first setup.

Interactive shortcuts:

| Key      | Action                                      |
| -------- | ------------------------------------------- |
| `r`      | Restart the app process.                    |
| `l`      | Toggle LAN mode with `.local` aliases.      |
| `b`      | Background the app and leave routes active. |
| `Ctrl-C` | Stop and unregister the app.                |

If the CLI detaches or backgrounds the app, the daemon keeps state. Running `tako dev` again for the same config attaches to the existing session when it is running or idle.

## Deploy Flow

`tako deploy` targets `production` unless `--env` is provided. The target environment must exist in `tako.toml`, must define `route` or `routes`, and cannot be `development`.

The deploy flow is:

1. Validate config, routes, secrets, and server target metadata.
2. Resolve the source root: git root when available, otherwise the app directory.
3. Resolve the app subdirectory from the selected config file.
4. Resolve runtime, preset, entrypoint, assets, package manager, and runtime version.
5. Copy source into `.tako/build`, respecting `.gitignore`, and symlink `node_modules` from the source tree.
6. Run build stages, merge assets into `public/`, and verify the resolved `main`.
7. Write `app.json`, archive the build output, and cache target artifacts under `.tako/artifacts/`.
8. Upload artifacts to every target server, extract, and run production install.
9. Run the optional `release` command once on the leader server.
10. Roll each server forward with health-checked replacement instances.

Servers receive prebuilt artifacts. They do not run app builds during deploy.

## Routing

Routes live under `[envs.<env>]` in `tako.toml`. They support exact hosts, wildcard subdomains, host plus path, and wildcard plus path:

```toml
[envs.production]
routes = [
  "example.com",
  "*.example.com/admin/*",
  "api.example.com/v1/*",
]
```

The proxy selects the most specific match by host and path. Static asset requests are served directly from the deployed `public/` directory when possible, then unmatched paths are proxied to the app. Unmatched hosts return `404`.

`/_tako/*` is reserved for Tako-owned public endpoints after a request has matched an app route. This includes durable channels and public image optimization.

## TLS

Production TLS uses SNI. Tako looks up an exact certificate first, then wildcard certificates. If no certificate exists yet, it serves a fallback self-signed certificate so the TLS handshake can finish and routing can return a normal HTTP response.

Tako issues certificates automatically:

- HTTP-01 for ordinary hostnames
- DNS-01 for wildcard routes after `tako servers configure <name>`
- self-signed certs for private or local names such as `localhost`, `.local`, `.test`, `.invalid`, `.example`, and `.home.arpa`

Certificates renew before expiry without stopping traffic.

## Instances And Scaling

Desired instance counts are runtime state on each server, not `tako.toml` config.

New deployments start with one warm instance per server. Use `tako scale` to change the desired count:

```bash
tako scale 2 --env production
tako scale 0 --env production
```

When desired instances are greater than zero, Tako keeps at least that many healthy instances running. When desired instances are zero, the app scales to zero after the idle timeout and wakes on the next request. The request waits for startup readiness; if startup times out, the proxy returns a generic `504`, and detailed diagnostics go to the app log stream.

Rolling updates start a new instance, wait for health, add it to the load balancer, drain an old instance, and repeat until the server has moved to the new release.

## Environment And Secrets

Non-secret variables come from `[vars]`, `[vars.<env>]`, and Tako runtime variables. Later layers override earlier ones. `ENV` is reserved and always derived by Tako.

HTTP instances and workflow workers receive the same app/runtime environment except for HTTP-only bind variables such as `PORT` and `HOST`.

Secrets are encrypted locally in `.tako/secrets.json`, with keys stored outside the repo. Storage credentials are encrypted locally in `.tako/storages.json` with the same environment-key mechanism. On deploy, secrets and storage bindings are stored encrypted in server SQLite and delivered to fresh app and worker processes through fd 3, not environment variables. Secret updates roll HTTP instances and restart workflow workers so new processes receive the latest values.

## Images And Storage

Public image optimization uses CDN-friendly query URLs:

```text
/_tako/image?src=/assets/hero.jpg&w=1200
```

Local public paths are available by default. Remote images must match `[images].remote_patterns` in `tako.toml`, protocol-less remote patterns allow both `http` and `https`, and widths, qualities, and formats must match the app's configured guardrails. JavaScript apps can use `imageUrl` for one optimized URL or `imageSrcSet` for plain `<img>` responsive sources.

Object storage bindings are attached with `tako storages add` and exposed to JavaScript apps as `tako.storages.<name>`. The SDK can create private signed download/upload URLs, and can build public optimized image URLs and responsive sources for storage objects when the binding has a `public_base_url`.

## Channels And Workflows

JavaScript channel files live under `<app_root>/channels/`. Workflow files live under `<app_root>/workflows/`. `app_root` defaults to `src`.

Channels are durable pub-sub streams served at:

```text
/_tako/channels/<name>
```

SSE channels support replay and live tail. WebSocket channels accept client messages, route them through the declared handler, and fan out returned messages.

Workflows are durable background runs stored in per-app SQLite. `tako-server` owns the database and worker supervision; SDKs talk to it through the internal socket. Workflows support retries, `ctx.run` checkpoints, delayed runs, cron schedules, `ctx.sleep`, `ctx.waitFor`, and `signal`.

In dev, workflow workers are scale-to-zero subprocesses so code changes apply on the next enqueue.

## Observability

App logs include stdout/stderr plus app-scoped Tako diagnostics. `tako logs` can read history or stream live logs, and `--json` emits JSONL for automation.

`tako-server` exposes Prometheus metrics on `127.0.0.1:9898` by default. Metrics include proxied request counts and latencies, upstream latency, active connections, cold starts, TLS handshake failures, instance health, and running instance counts.
