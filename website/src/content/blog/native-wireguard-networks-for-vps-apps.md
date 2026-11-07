---
title: "Native WireGuard Networks for VPS Apps with Tako"
date: "2026-11-06T15:14"
description: "Give VPS apps a private WireGuard network, deploy from CI over pinned HTTPS, and choose exactly which machines can manage a release."
image: 01e13c209f85
imageAlt: "A coral octopus lays a cable between server racks on two islands."
---

A CI runner needs to upload a release. An operator needs a shell on the VPS. Two app instances need to exchange events. All three need to know which machine they are talking to, but only two need a private IP route. Tako's native networks let us give each of them an identity and grant the access its job requires.

Take a shop that runs on two VPSs. The shop's production network can contain both servers, a developer's laptop, and a build runner with permission to deploy just `shop/production`. The runner can work in a sandbox that only permits outbound HTTPS. The servers can talk to each other over WireGuard. Here's how those pieces fit together. [Native networks documentation](/docs/networks/)

## Two connections to the same VPS

During enrollment, each server generates its keys locally. The administrator approves its identity and records the server's WireGuard endpoint and HTTPS certificate information in network membership. From then on, the CLI can check the identity of the server answering a management request. A server receiving a signed request can check the caller and its permissions. The IP address tells the CLI where to connect. The pinned certificate tells it whom it reached.

```d2
direction: right
runner: "CI runner"
laptop: "Operator's laptop"
east: "Shop VPS · east"
west: "Shop VPS · west"
runner -> east: "signed HTTPS deploy"
laptop -> east: "SSH over WireGuard"
east <-> west: "private WireGuard traffic"
```

Consider what happens when you read yesterday's production logs. The CLI connects directly to the server's HTTPS port and checks its pin. The server checks whether this device may read logs for the requested app and environment. The runner can do this without creating a network interface, installing a VPN daemon, or gaining the privileges needed to change a routing table. This matters on a hosted runner where the build process has no access to a TUN device. [CLI reference](/docs/cli/)

A shell takes a different path. An administrator can approve a laptop for SSH, and `tako ssh` can connect to a server's private address using that device's SSH identity. The workstation needs WireGuard tools and permission to bring up the interface. You can test the public management connection and the private shell independently. A successful log request is evidence about HTTPS. A fresh WireGuard handshake followed by a successful SSH command is evidence about the private path.

| Operation | Connection | What to verify |
| --- | --- | --- |
| Enroll the first server | Administrator SSH to the Linux host | The expected server identity and endpoint enter membership |
| Deploy, read logs, inspect releases | Signed requests over pinned HTTPS, normally TCP 443 | The intended device can operate on the expected app and environment |
| Open a private shell | SSH over WireGuard, normally UDP 51820 on the outside | The intended VPS answers using its enrolled host key |
| Exchange private traffic between servers | WireGuard between reachable server endpoints | Each peer can reach the advertised address and UDP port |

## Choose which machines should receive a deploy

Think about placement before creating a network. A production environment that selects a native network targets its trusted server members. If the shop should run in two regions, enroll both shop servers. If a third VPS belongs to a different project with its own database and release schedule, give it a separate network. This choice determines what will happen the next time someone types `tako deploy`. A single-server network is a perfectly good choice.

On the administrator's workstation, choose a short local alias such as `shop`. In this example the VPS has a reachable public IP, `203.0.113.10` stands in for that address, and `ubuntu` is its administrator SSH user. Allow direct HTTPS to the server from the machines that manage it and inbound UDP on the WireGuard endpoint from its peers. On a hosted VPS, check both the provider firewall and Linux's input rules.

```bash
tako network create shop
tako servers add ubuntu@203.0.113.10 --name east --network shop \
  --wireguard-endpoint 203.0.113.10:51820
tako network status shop --json
```

Use the returned **network ID** in the project. The ID identifies the same network on every enrolled device, even if another developer chooses a different local alias. In the example below, substitute your actual ID and app hostname. The public route and the network answer separate questions. One names the address a customer visits. The other selects the machines that will receive a release. [Deployment configuration](/docs/deployment/) and [server requirements](/docs/servers/)

```toml
[envs.production]
network = "0123456789abcdef0123456789abcdef"
route = "shop.example.com"
```

Membership also supplies the network part of private service names. From a server with native service DNS configured, an app can address the shop as `shop.production.<network-id>.tako.internal`. The private router uses current authenticated app placements. The public domain can continue pointing at an existing CDN or load balancer. If a colleague wants to use that private hostname in a desktop browser, arrange DNS on that workstation separately. Bringing up WireGuard installs the peer routes. The CLI leaves workstation DNS configuration to the operator.

For two shop servers, that distinction extends to application data. Native channels can exchange live events and bounded replay between servers. Presence uses expiring records so clients can see who is connected. Adding the second server to membership does not copy a database file from the first server. Decide where orders are durable, how both regions read them, and what happens when a region goes offline before you start sending customers there. A private service name gives an application a route to another service. Its storage still needs its own design. [Channels](/docs/channels/) and [workflow storage](/docs/workflows/)

## Give a build runner one job

Suppose the shop's release pipeline needs one hour to ship a fix. Generate an enrollment request on the runner. The request is the piece to transfer to the administrator. The private signing and WireGuard keys stay on the machine that generated them.

```bash
# On the runner, using the shop's real network ID
tako network request --name shop-ci \
  --network 0123456789abcdef0123456789abcdef \
  --output shop-ci.request.json

# On an administrator's enrolled machine
tako network approve shop-ci.request.json --network shop \
  --deploy shop/production --expires-in 3600 \
  --output shop-ci.profile.json
```

Return the approved profile to the requesting runner through a trusted channel. It contains the information the runner uses to find the server and check its identity. Import checks that the local keys match the original request. Copying an approved profile onto a different build agent will therefore fail. A replacement runner should make its own request, which also gives you a distinct device ID to revoke later.

```bash
# Back on the same runner, in the shop project
tako network import shop-ci.profile.json --name shop
tako network status shop --json
tako deploy --env production --ci
```

Before handing this job to CI, have an administrator provision the app's public route and any required server-level storage or credential bindings. A deployment grant authorizes the named workload. It leaves global ingress changes and shell access under separate permissions. App secret keys have their own distribution process as well. If the job cannot decrypt an environment secret, check its access to that environment's key. Granting network administration would give the job more authority without supplying the missing key. [Secrets and key sharing](/docs/secrets/)

For a first rollout, pick one host and one read-only operation you already know how to check. Look up the running release through native management, open a new private shell if you need one, and request a familiar public URL. Each check answers a different operational question. If the VPS already uses Tailscale, keep that SSH session open as you work through the [Tailscale migration guide](/blog/migrate-tailscale-to-tako-wireguard/).
