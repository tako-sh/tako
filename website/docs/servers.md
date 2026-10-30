---
title: Servers
description: "Provision and enroll Tako servers over Tailscale or NetBird, map environments to hosts, and maintain the service with reloads and verified upgrades."
---

A server is a Linux host running `tako-server`. Your workstation keeps a global server inventory; each app's `tako.toml` maps deployment environments to inventory names. Normal app operations use signed HTTP over the selected Tailscale or NetBird network. SSH handles bootstrap, recovery, and maintenance.

## Server Setup

Install the server on a Linux host with glibc and systemd, using x86_64 or ARM64. Alpine/musl and OpenRC hosts are not supported.

Run the installer:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

Custom public ports:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo sh -s -- --http-port 8080 --https-port 8443
```

The installer creates the `tako` service user, the shared `tako-app` group, `/opt/tako`, `/var/run/tako`, service files, maintenance helpers, restricted sudoers policy, public HTTP/HTTPS listeners, local metrics, libvips runtime support, and private management over Tailscale or NetBird.

On Linux, each app/environment runs as a separate Unix identity. Apps can write their own release files and data, including dependencies, while the service protects the release manifest and control directories. The internal socket checks the connecting Unix identity against the requested app. Image decoding runs under a separate unprivileged account with bounded resources.

Production isolation requires cgroup v2 with CPU, memory, and process controllers. Each app is limited to 2 GiB of memory, two CPUs, and 512 processes. Missing identity provisioning or resource enforcement stops app startup. The installer restarts an existing service so children launched under an older policy are replaced. If you use `TAKO_RESTART_SERVICE=0` when upgrading, restart the service before relying on the updated isolation policy.

Normal installs require [Tailscale](https://tailscale.com/) or [NetBird](https://netbird.io/) for remote management. Install the VPN and its CLI on both your workstation and server, connect them to the same network, and allow SSH and TCP port `9844` between them in the VPN's access policies and host firewall. Use the server's VPN hostname or IP. Tako verifies the selected agent's connection and address ownership, verifies SSH recovery access, and enrolls the SSH key for signed management.

Tako uses an already-connected VPN. Account creation, peer enrollment, DNS, and access policies are managed with your VPN provider, including self-hosted installations.

## Add A Server

```bash
tako servers add my-server.tailnet.ts.net
```

or:

```bash
tako servers add ubuntu@my-server.tailnet.ts.net
```

Interactive setup checks the management endpoint and `tako@host` SSH access. If the server is running but inaccessible, Tako offers to repair access; if its state is unknown, Tako offers to install or repair it. After confirmation, Tako asks for an administrator with `root` pre-filled. Use `--install` to skip the confirmation or `admin@host` to select the administrator without a prompt. Public HTTP and HTTPS ports default to `80` and `443`; override them with `--http-port` and `--https-port`. The add flow stores the private network provider, host, SSH port, public ports, description, and detected target metadata (`arch`, `libc`).

### Choose the private network

Setup asks which private network connects you to the server. Connected providers appear first with a `detected` hint, and the first option is preselected. Detection checks the running agent; installing a VPN client alone does not mark it as detected.

For scripts, select the provider explicitly:

```bash
tako servers add ubuntu@my-server.netbird.cloud --network netbird
tako servers add ubuntu@my-server.tailnet.ts.net --network tailscale
```

Use your server's actual VPN name. Without a flag, an existing host keeps its saved provider. For new hosts in noninteractive mode, Tako uses the only connected provider, defaults to Tailscale if neither is detected, and requires `--network` if both are connected. Connection checks still run unless you explicitly pass `--no-test`.

The provider is stored in the global `config.toml` server inventory:

```toml
[[servers]]
name = "prod"
host = "my-server.netbird.cloud"
network = "netbird"
```

Entries without `network` use Tailscale. Repair and upgrades preserve the server's saved provider, even when both VPNs are installed. To migrate an existing entry to another VPN, configure the server on the new network as administrator, then remove and re-add the local entry.

### Install directly on the server

For a direct NetBird installation, run the installer as root with `TAKO_NETWORK=netbird`:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo env TAKO_NETWORK=netbird sh
```

The standalone installer otherwise uses its saved provider, then Tailscale. `TAKO_MANAGEMENT_HOST` can select an address assigned to this server by that provider. It cannot select an unrelated LAN or public address.

Management binds only to the selected VPN address on port `9844`. Private custom NetBird IPv4 ranges and IPv6 ULA addresses are accepted when reported by the selected agent. Public app traffic continues to use the configured HTTP/HTTPS listeners.

## Server Maintenance

```bash
tako servers reload la
tako servers reload la --force
tako servers upgrade la
tako servers upgrade la --force
tako servers uninstall la --yes
```

`reload` is zero-downtime by default. Its `--force` flag performs a restart. `upgrade` installs a candidate `tako-server` binary, enters upgrade mode, and checks its protocol against the CLI and every active release before reloading. It checks again after readiness. A mismatch fails before reload; any failure after reload begins restores and restarts the previous binary before upgrade mode ends. `upgrade --force` attempts a protocol mismatch but keeps readiness and rollback enabled. `uninstall` removes the remote service and data, then removes the local server entry.

Maintenance runs through restricted, root-owned helpers. Upgrades accept only official releases: the CLI verifies the signed release manifest, and the host verifies the archive against that manifest before installation. Install custom builds as administrator; `servers upgrade` rejects `TAKO_DOWNLOAD_BASE_URL` overrides.

## Map an app to servers

```toml
[envs.production]
route = "app.example.com"
servers = ["la", "nyc"]
```

Inventory names must exist in `tako servers list`. Removing an inventory entry with `tako servers remove` forgets the host locally; uninstalling the server removes its remote service and data. See [Deployment](/deployment/#configure-an-environment) for environment selection, and configure shared primitive state before deploying to multiple servers.

## Verify access

```bash
tako servers list --json
tako status --json
```

Check the selected host, `network`, target architecture/libc metadata, and management reachability before deploying. After a service upgrade, inspect status and request an existing app route. See [Troubleshooting](/troubleshooting/#server-commands-fail) for repair paths.
