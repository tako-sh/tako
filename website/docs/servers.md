---
title: Servers
description: "Enroll Tako network servers, select deployment targets, and maintain the service."
---

A server is a Linux host running `tako-server`. [Native networks](/networks/) enroll servers and devices with their own keys, resolve trusted membership, and manage apps over pinned HTTPS. Start there for the create, server-add, and environment-selection workflow.

## Server Setup

Install the server on a Linux host with glibc and systemd, using x86_64 or ARM64. Alpine/musl and OpenRC hosts are not supported.

For a stopped bootstrap installation, run the installer, then complete enrollment from your workstation:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo env TAKO_RESTART_SERVICE=0 sh
```

Custom public ports:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo env TAKO_RESTART_SERVICE=0 sh -s -- --http-port 8080 --https-port 8443
```

The installer creates the `tako` service user, the shared `tako-app` group, `/opt/tako`, `/var/run/tako`, service files, maintenance helpers, restricted sudoers policy, public HTTP/HTTPS listeners, local metrics, libvips runtime support, and native WireGuard networking and signed, pinned HTTPS management.

On Linux, each app/environment runs as a separate Unix identity. Apps can write their own release files and data, including dependencies, while the service protects the release manifest and control directories. The internal socket checks the connecting Unix identity against the requested app. Image decoding runs under a separate unprivileged account with bounded resources.

Production isolation requires cgroup v2 with CPU, memory, and process controllers. Each app is limited to 2 GiB of memory, two CPUs, and 512 processes. Missing identity provisioning or resource enforcement stops app startup. The installer restarts an existing service so children launched under an older policy are replaced. If you use `TAKO_RESTART_SERVICE=0` when upgrading, restart the service before relying on the updated isolation policy.

## Add A Server

```bash
tako network create production
tako servers add ubuntu@203.0.113.10 --network production --name edge
tako network status production --json
```

Replace the example IP with your server's reachable address. Bootstrap installs the native network components over administrator SSH, generates server keys on the host, enrolls its membership, and starts the service. Without an explicit SSH user, Tako uses `root`. With no host in an interactive terminal, Tako prompts for `admin@host`.

`--network` selects a local alias or stable ID. If omitted, Tako uses the project's production network (including a file selected with `--config`) or the only configured network. An explicit `--network` takes precedence. Create or import a network first; multiple networks require an explicit selection. `--ssh-key` selects the bootstrap key. The bootstrap SSH host and the key's absolute path are saved locally for subsequent reload, upgrade, and uninstall commands on this workstation. These SSH settings are scoped to the server and local network identity and are not exported with network membership. Use `--port` for a custom SSH port and `--http-port` / `--https-port` for public listeners (defaults `80` / `443`). `--wireguard-endpoint` overrides the advertised IP and UDP port (default: the host's IP on UDP `51820`); it does not change the saved SSH host.

Allow the configured HTTPS port for management and the WireGuard UDP endpoint for private traffic. Deployments use signed pinned HTTPS without requiring a workstation WireGuard interface. See [Native networks](/networks/) for enrollment, permissions, routing, and connectivity limits.

### Choose the VPN provider

Tako manages its own WireGuard network. Provider selection and Tailscale/NetBird integrations have been removed. The `--vpn`, `--no-test`, `--install`, and server `--description` flags are no longer available.

To replace an old installation, create a Tako network, re-enroll each server with the command above, and set `[envs.<env>].network` to its stable ID. Remove the old `[[servers]]` entries from your local inventory after enrollment. To preserve placement in each project, set the environment’s `servers` list to the corresponding enrolled server names. Saved `vpn` fields fail with re-enrollment guidance; they are not silently reassigned.

### Install directly on the server

For an image build or a stopped bootstrap installation:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo env TAKO_RESTART_SERVICE=0 sh
```

The installer defaults to native networking. Service startup requires applied native network state; finish enrollment from the administrator's workstation. `TAKO_MANAGEMENT_HOST` may select only an address assigned by that state. Saved external-provider settings are rejected during standalone upgrades; administrator re-enrollment explicitly selects native networking.

## Server Maintenance

```bash
tako servers reload production/edge
tako servers reload production/edge --force
tako servers upgrade production/edge
tako servers upgrade production/edge --force
tako servers uninstall production/edge --yes
```

Use the `alias/server-name` shown by `servers list`. `reload` is zero-downtime by default. Its `--force` flag performs a restart. `upgrade` installs a candidate `tako-server` binary, enters upgrade mode, and checks its protocol against the CLI and every active release before reloading. It checks again after readiness. A mismatch fails before reload; any failure after reload begins restores and restarts the previous binary before upgrade mode ends. `upgrade --force` attempts a protocol mismatch but keeps readiness and rollback enabled. `uninstall` first publishes a revocation of the server's network membership and delegated approvals, then removes the remote service and data over the existing SSH session. If revocation cannot be confirmed, uninstall fails and leaves the remote files intact. Offline peers enforce the revocation when they reconnect. If remote cleanup fails after revocation, uninstall reports the remaining SSH cleanup instead of success.

`servers remove <alias/server-name>` revokes the server's network membership and delegated approvals after confirmation. Enforcement on offline peers remains pending until they reconnect. It does not uninstall the service or delete app data.

Maintenance runs through restricted, root-owned helpers. Upgrades accept only official releases: the CLI verifies the signed release manifest, and the host verifies the archive against that manifest before installation. Install custom builds as administrator; `servers upgrade` rejects `TAKO_DOWNLOAD_BASE_URL` overrides.

## Map an app to servers

Copy the stable ID from `tako network list --json`:

```toml
[envs.production]
route = "app.example.com"
network = "0123456789abcdef0123456789abcdef"
```

Use the actual network ID. Trusted members supply deployment targets. See [Deployment](/deployment/#configure-an-environment), and configure shared primitive state before deploying to multiple servers.

## Verify access

```bash
tako servers list --json
tako status --json
```

Check the selected host, network membership, and management reachability before deploying. After a service upgrade, inspect status and request an existing app route. See [Troubleshooting](/troubleshooting/#server-commands-fail) for repair paths.
