---
title: Servers
description: "Provision and enroll Tako servers over Tailscale, map environments to hosts, and maintain the service with reloads and verified upgrades."
---

A server is a Linux host running `tako-server`. Your workstation keeps a global server inventory; each app's `tako.toml` maps deployment environments to inventory names. Normal app operations use signed HTTP over private Tailscale addresses. SSH handles bootstrap, recovery, and maintenance.

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

The installer creates the `tako` service user, the shared `tako-app` group, `/opt/tako`, `/var/run/tako`, service files, maintenance helpers, restricted sudoers policy, public HTTP/HTTPS listeners, local metrics, libvips runtime support, and private Tailscale management.

On Linux, each app/environment runs as a separate Unix identity. Apps can write their own release files and data, including dependencies, while the service protects the release manifest and control directories. The internal socket checks the connecting Unix identity against the requested app. Image decoding runs under a separate unprivileged account with bounded resources.

Production isolation requires cgroup v2 with CPU, memory, and process controllers. Each app is limited to 2 GiB of memory, two CPUs, and 512 processes. Missing identity provisioning or resource enforcement stops app startup. The installer restarts an existing service so children launched under an older policy are replaced. If you use `TAKO_RESTART_SERVICE=0` when upgrading, restart the service before relying on the updated isolation policy.

Normal installs require Tailscale for remote management. The CLI expects a Tailscale MagicDNS host or Tailscale IP, verifies SSH recovery access, enrolls the SSH key for signed management, and then uses signed HTTP for app operations.

## Add A Server

```bash
tako servers add my-server.tailnet.ts.net
```

or:

```bash
tako servers add ubuntu@my-server.tailnet.ts.net
```

Interactive setup checks the management endpoint and `tako@host` SSH access. If the server is running but inaccessible, Tako offers to repair access; if its state is unknown, Tako offers to install or repair it. After confirmation, Tako asks for an administrator with `root` pre-filled. Use `--install` to skip the confirmation or `admin@host` to select the administrator without a prompt. Public HTTP and HTTPS ports default to `80` and `443`; override them with `--http-port` and `--https-port`. The add flow stores host, SSH port, public ports, description, and detected target metadata (`arch`, `libc`).

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

Check the selected host, target architecture/libc metadata, and management reachability before deploying. After a service upgrade, inspect status and request an existing app route. See [Troubleshooting](/troubleshooting/#server-commands-fail) for repair paths.
