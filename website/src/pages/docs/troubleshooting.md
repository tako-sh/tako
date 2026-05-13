---
layout: ../../layouts/DocsLayout.astro
title: "Troubleshooting deploy failures, TLS issues, and runtime errors - Tako Docs"
heading: Troubleshooting
current: troubleshooting
description: "Troubleshoot common Tako problems including deploy failures, TLS issues, runtime errors, server status problems, and verbose diagnostics."
---

# Troubleshooting

Start with these commands:

```bash
tako doctor
tako servers status
tako logs --env production
tako deploy --verbose
```

Use `--ci` for deterministic output in automation and `--json` for machine-readable logs:

```bash
tako logs --env production --json
```

## Config Not Found

App-scoped commands read `./tako.toml` by default.

```bash
tako deploy -c path/to/app.toml
```

If the config path has no `.toml` suffix, Tako adds it. The selected config file's parent directory is the app directory.

## Invalid App Name

App names must start with a lowercase letter and contain only lowercase letters, numbers, and hyphens.

```toml
name = "dashboard"
```

If `name` is omitted, Tako derives the name from the selected config file's parent directory. Set `name` explicitly before deploying long-lived apps so renames do not create new server-side app identities.

## Environment Not Found

Deploy, logs, releases, delete, and scale all resolve an environment.

```toml
[envs.production]
route = "dashboard.example.com"
servers = ["la"]
```

`production` is the default. `development` is reserved for `tako dev` and cannot be deployed.

## Route Problems

Use either `route` or `routes`, not both:

```toml
[envs.production]
routes = [
  "example.com",
  "*.example.com/admin/*",
  "example.com/api/*",
]
```

Non-development environments must define at least one route.

If two apps claim the same route on a server, deploy fails during route conflict checks. Use `tako servers status` to see what is already deployed.

## No Servers Configured

Add a server:

```bash
tako servers add host.example.com --name la
```

If the server is not installed:

```bash
tako servers add ubuntu@host.example.com --install --name la
```

If the host cannot use public `80`/`443`, pass custom proxy ports during install:

```bash
tako servers add ubuntu@host.example.com --install --name la --http-port 8080 --https-port 8443
```

Then map the environment:

```toml
[envs.production]
route = "dashboard.example.com"
servers = ["la"]
```

## Missing Target Metadata

Deploy requires `arch` and `libc` metadata for every target server. `tako servers add` records it after a successful SSH check.

Repair by re-adding the server with checks enabled:

```bash
tako servers add host.example.com --name la
```

Avoid `--no-test` unless you know deploy will not target that server yet.

## SSH Auth Problems

`servers add`, deploy upload, install, and logs use SSH. If your local private key is encrypted, pass:

```bash
tako --ssh-passphrase "$PASSPHRASE" servers add host.example.com --name la
```

For remote management HTTP, Tako signs requests with local SSH keys or ssh-agent keys. `servers add --install` enrolls the key that authenticated the admin SSH connection.

Custom public HTTP ports do not change remote management; management still uses private port `9844` over Tailscale.

## Remote Management Fails

`tako servers status` uses remote management HTTP on port `9844`. Public probes are `hello` and `server_info`; other RPCs require signed headers.

Check:

- the host is reachable over your private network
- `tako-server` is running
- the management key was enrolled
- your local SSH key or ssh-agent can sign requests

Run:

```bash
tako servers status --verbose
```

## Deploy Confirmation Surprises

Interactive production deploy prompts only when the environment is implicit:

```bash
tako deploy
```

These skip the prompt because the target is explicit:

```bash
tako deploy --env production
tako deploy --env production --yes
```

## Build Fails

Check the build config:

```toml
[build]
install = "bun install"
run = "bun run build"
```

For monorepos, use `cwd` or `[[build_stages]]`.

```toml
[[build_stages]]
name = "web"
cwd = "packages/web"
run = "bun run build"
```

`[build]` and `[[build_stages]]` are mutually exclusive when `[build].run` is set. `[build].include` and `[build].exclude` cannot be used with `[[build_stages]]`.

## Missing `main`

Deploy and dev need a runtime entrypoint. Set it explicitly when detection cannot find one:

```toml
main = "dist/server/tako-entry.mjs"
```

Tako checks top-level `main`, manifest metadata such as `package.json` `main`, then preset/runtime defaults.

For Vite/TanStack Start, ensure `tako.sh/vite` is installed so the deploy wrapper is emitted. For Next.js, use `withTako()` from `tako.sh/nextjs`.

## Runtime Version Problems

Pin the runtime version when auto-detection is not what you want:

```toml
runtime = "bun"
runtime_version = "1.2.3"
```

Without a pin, deploy runs `<runtime> --version` locally and falls back to `latest`.

## Production Install Fails

Servers run production install after extracting the artifact. The command comes from the runtime plugin and package manager.

For JS apps, make sure the deploy artifact includes the lockfile and package metadata needed by the selected package manager.

Production install runs from a cleared `tako-server` service environment. Tako preserves only `PATH` and `HOME` when available, then applies release env. Put required app configuration in `[vars]`, `[vars.<env>]`, or secrets.

If the server reports that `tako-app` cannot be resolved, repair the server install so the `tako-app` OS user exists. A root `tako-server` will not run production install as root.

## Release Command Fails

Run with verbose output:

```bash
tako deploy --env production --verbose
```

The release command runs once on the leader server after extract and production install, before rolling update. It receives app env, deploy secrets, `TAKO_BUILD`, `TAKO_DATA_DIR`, and `PATH` when absent from the release env. It starts from a cleared service environment.

Failures, non-zero exits, and 10-minute timeouts abort deploy. Old instances keep serving.

## App Does Not Become Healthy

Tako waits for the SDK to write its bound port to fd 4, then probes `/status` with `Host: <app>.tako`.

Check:

- the app imports and uses the Tako SDK entrypoint or Go SDK
- the app does not bind its own fixed public port
- startup does not block before the server begins listening
- secrets and required env vars are configured

Use logs:

```bash
tako logs --env production
```

Startup timeout diagnostics include captured startup stdout/stderr when available.

## Scale-To-Zero Cold Starts

When desired instances are `0`, the next request wakes the app. If readiness does not happen before the startup timeout, the proxy returns `504`. If startup setup fails, it returns `502`.

Increase desired instances to keep the app hot:

```bash
tako scale 1 --env production
```

## Local `.test` Does Not Resolve

Run:

```bash
tako doctor
```

On macOS, Tako manages `/etc/resolver/test`, `/etc/resolver/tako.test`, a local DNS listener, and a launchd-managed loopback proxy. If `/etc/resolver/test` already exists and was not created by Tako, `.tako.test` remains available as a fallback.

On Linux, Tako configures systemd-resolved and loopback redirect rules. On NixOS, it prints a configuration snippet instead of making imperative changes.

## Local HTTPS Shows Certificate Errors

Tako creates a local root CA per `{TAKO_HOME}` and installs it into system trust. The CA files are:

```text
{TAKO_HOME}/ca/ca.crt
{TAKO_HOME}/ca/ca.key
```

Run `tako doctor` and then `tako dev` again to repair trust setup.

## Vite Dev Never Starts

Use `tako.sh/vite` in your Vite config. Tako does not parse Vite stdout URLs as readiness; the plugin is responsible for the fd-4 readiness handshake.

For Bun, built-in presets use:

```toml
dev = ["bun", "--bun", "./node_modules/.bin/vite", "dev"]
```

This avoids shims that drop file descriptors above 2.

## LAN Mode Does Not Show Wildcards

mDNS cannot advertise wildcard records. LAN mode advertises concrete `.local` hostnames only.

Add explicit development routes for devices that need mDNS discovery:

```toml
[envs.development]
routes = ["api.app.test"]
```

## Wildcard Certificates Fail

Configure DNS-01 support:

```bash
tako servers setup-wildcard
```

The command currently applies to all configured servers. It accepts `--env`, but that flag does not filter targets.

Credentials are written to `/opt/tako/dns-credentials.env`, and the provider is stored in `/opt/tako/config.json`.

## Logs Are Empty

Check that the environment maps to the server where the app is deployed:

```toml
[envs.production]
servers = ["la"]
```

Fetch more history:

```bash
tako logs --env production --days 14
```

Stream live logs:

```bash
tako logs --env production --tail
```

## Secrets Missing At Runtime

Set and sync:

```bash
tako secrets set DATABASE_URL --env production
tako secrets sync --env production
```

Then redeploy or let the sync trigger fresh processes. App and worker processes receive secrets through fd 3 at startup; a running process does not mutate its in-memory secret bag.

## Roll Back

List releases:

```bash
tako releases ls --env production
```

Roll back:

```bash
tako releases rollback abc1234 --env production --yes
```

Rollback uses the normal rolling-update path and keeps current routes, env, secrets, and scaling config.

## Reset Local Dev State

Stop registered dev apps:

```bash
tako dev stop --all
```

Remove local Tako data and dev system configuration:

```bash
tako implode
```

`tako implode` may require sudo for platform services, resolver files, trust-store entries, proxy services, and loopback aliases.
