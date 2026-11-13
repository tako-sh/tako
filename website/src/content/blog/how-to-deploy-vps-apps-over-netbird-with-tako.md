---
title: "How to Deploy VPS Apps over NetBird with Tako"
date: "2026-11-02T03:09"
description: "Deploy VPS apps over NetBird with Tako: connect peers, configure private management, enroll SSH keys, and verify public HTTPS."
image: 4986a7db690f
imageAlt: "A coral octopus connects a laptop to a server rack through a glowing NetBird cable."
---

> **Update, November 13, 2026:** This article describes an earlier Tako release. Server commands now use `tako network server`; native network membership supplies the inventory. See the [current enrollment instructions](/docs/networks/).

> **Networking update.** The Tailscale and NetBird integrations and older setup commands described here have been retired. For new installations, see [native WireGuard networks](/blog/native-wireguard-networks-for-vps-apps/). For an existing Tailscale or NetBird host, follow the [migration guide](/blog/migrate-tailscale-to-tako-wireguard/).

Your laptop and VPS already share a NetBird network. You can reach the server privately, and you want app deploys to follow that same path. Tako now supports NetBird for remote management: enroll the host once, map it to your app, and deploy over the private connection.

We'll use an existing Tako app and a Linux VPS. By the end, your CLI will manage the server through NetBird while visitors reach the app through its public HTTPS hostname. Two addresses, two jobs. The DNS records get to keep their day jobs.

## Connect the laptop and VPS through NetBird

Start with NetBird installed and connected on both machines, including its CLI. Tako uses the network you have already configured; peer enrollment, DNS, and access policies remain in NetBird. This applies to an existing hosted or self-hosted NetBird installation. If you are still setting that up, follow [NetBird's getting started guide](https://docs.netbird.io/get-started/cli) first.

For the VPS, use a supported Linux host with glibc and systemd, on x86_64 or ARM64. Production app isolation also requires cgroup v2 with CPU, memory, and process controllers. The [server setup guide](/docs/servers/#server-setup) covers those requirements. Have an administrator SSH account ready; we'll use `ubuntu` below, but substitute the account your host provides.

Run this on your workstation and on the VPS:

```bash
netbird status
```

Confirm the agent is connected. For peer details, run:

```bash
netbird status -d
```

[NetBird's CLI reference](https://docs.netbird.io/get-started/cli) documents both commands. Find your server's actual peer hostname or NetBird IP. We'll use `my-server.netbird.cloud` as a placeholder throughout; replace it everywhere with the name or address reported for your server. Self-hosted installations may use a different DNS suffix.

Keep that private hostname separate from the app's public domain:

| Address | Example | Purpose |
| --- | --- | --- |
| Server's NetBird hostname | `my-server.netbird.cloud` | SSH and Tako management from your workstation |
| Public app hostname | `app.example.com` | HTTPS requests from visitors |
| Local inventory name | `prod` | Reference to the server in your app configuration |

In [NetBird access policies](https://docs.netbird.io/manage/access-control), allow the workstation peer or its operator group to reach the VPS peer on TCP `9844` and the server's SSH port, normally TCP `22`. Ensure the host firewall permits those connections over the VPN too. This tutorial uses the host's ordinary SSH service and your SSH keys over NetBird.

The public app has a separate network requirement: visitors must reach its HTTP and HTTPS listeners, normally ports `80` and `443`. Port `9844` belongs on the private network. The [Servers reference](/docs/servers/) describes that listener boundary.

```d2
direction: right

laptop: "Your laptop\nTako CLI" {
  style.fill: "#9BC4B6"
}

vpn: "NetBird\nprivate network" {
  style.fill: "#FFF9F4"
}

vps: "Your VPS" {
  management: "Private management\nTCP 9844"
  ssh: "SSH\nsetup and recovery"
  public: "Public HTTPS\nTCP 443" {
    style.fill: "#E88783"
  }
  app: "Your application"
  public -> app
}

visitors: "Visitors"
laptop -> vpn
vpn -> vps.management: "signed requests and uploads"
vpn -> vps.ssh: "SSH"
visitors -> vps.public: "app.example.com"
```

The diagram shows the separation you'll verify below: a working VPN connection establishes management reachability; a successful public request establishes that the application is reachable by its users.

## Enroll the server and deploy your app

Install the Tako CLI on your workstation if needed:

```bash
curl -fsSL https://tako.sh/install.sh | sh
tako --version
```

Then add the server, choosing NetBird explicitly and giving the inventory entry a short name:

```bash
tako servers add ubuntu@my-server.netbird.cloud --vpn netbird --name prod
```

The `ubuntu@` prefix selects the administrator for installation or repair. Interactive setup checks the existing service and recovery access, offers installation or repair when needed, and enrolls your SSH key for signed management. The installer creates the Tako service account, service files, and supporting directories. You can follow the prompts rather than install the server separately first.

Selecting `--vpn netbird` makes the choice explicit even when Tailscale is also connected. Without the flag, interactive setup presents the providers with connected ones first. In noninteractive runs, two connected providers require an explicit selection. Installing a VPN executable alone does not count as a connected agent.

If you prefer to install directly from an administrator shell on the VPS, the equivalent NetBird-specific installer invocation is:

```bash
curl -fsSL https://tako.sh/install-server.sh | sudo env TAKO_VPN=netbird sh
```

Connect the VPS to NetBird before running it. Then return to your workstation and run the same `tako servers add` command to enroll the host locally. The standalone installer uses its saved provider when available, otherwise Tailscale, so include `TAKO_VPN=netbird` for a fresh NetBird install.

During enrollment, Tako checks the selected VPN agent and verifies that the destination belongs to it. The server-side check verifies the address is assigned to that server. A random private IP is insufficient. Normal management then uses signed HTTP on the selected VPN address; SSH remains available for bootstrap, recovery, and maintenance. The [implementation is open source](https://github.com/tako-sh/tako/blob/master/tako/src/vpn.rs).

Check the saved entry:

```bash
tako servers list --json
tako status --json
```

Find `prod`, confirm its host and `vpn` value, and check management reachability. Tako stores the provider in its global server inventory. Your app only needs to reference the inventory name.

Inside an existing Tako app, set the production environment in `tako.toml`, preserving its runtime, build, and entrypoint configuration:

```toml
[envs.production]
route = "app.example.com"
servers = ["prod"]
```

For a project without Tako configuration, start with [`tako init`](/docs/quickstart/) and the relevant [framework guide](/docs/framework-guides/). Here we're adding a deployment host to an app that's already configured.

Point `app.example.com` at the VPS's public address in your public DNS. Declaring a route does not create that record. For this direct-to-VPS example, leave the default Let's Encrypt mode and make public port `80` reachable for HTTP-01 validation. See [Routing and TLS](/docs/routing/) for other certificate arrangements.

Now deploy:

```bash
tako deploy --env production
```

Tako validates the configuration, builds and packages the release locally, uploads it through signed private HTTP management, and prepares a rolling update on the server. Replacement instances must become healthy before receiving public traffic. The [deployment guide](/docs/deployment/) covers that sequence and failure handling.

## Verify the public app and keep recovery working

After deployment, inspect the running state, release history, and recent logs:

```bash
tako status --json
tako releases list --env production --json
tako logs --env production --days 1 --json
curl -i https://app.example.com/
```

Use an actual application endpoint if `/` isn't meaningful. Confirm the certificate and expected response, then exercise a representative user flow. A completed rollout establishes that the release started; the application request checks the behavior users receive. The [operations guide](/docs/operations/) explains these finite inspection commands.

If a check fails, follow the boundary that failed:

| Symptom | First check |
| --- | --- |
| VPN provider is unavailable | Run `netbird status` on both machines; confirm their agents are connected. |
| Hostname fails verification | Compare its resolved destination with the server's actual NetBird address. |
| SSH setup fails | Check the administrator, SSH key, SSH port, access policy, and host firewall. |
| Management is unreachable | Check private TCP `9844`, the selected provider, and the running Tako service. |
| Public HTTPS fails | Check public DNS, listener reachability, certificate setup, and app logs. |

Repairs and upgrades preserve the inventory's VPN choice. Keep administrator SSH access available for recovery, and use the [server maintenance guide](/docs/servers/#server-maintenance) when the service itself needs attention.

For an existing Tailscale entry, migration requires configuring the server on NetBird as administrator, then removing and re-adding the local inventory entry. Removal forgets the local entry; server uninstall removes the remote service and data. Follow the [provider selection guide](/docs/servers/#choose-the-vpn-provider) and retain the inventory name referenced by your apps.

You now have a repeatable path from your laptop to a public app: NetBird supplies private connectivity, your enrolled key authenticates management, and Tako handles the release and HTTPS routing. The earlier [Tailscale management post](/blog/self-hosted-deployments-over-tailscale/) explains the signing model in more depth; this setup brings the same workflow to your existing NetBird network.
