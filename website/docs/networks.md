---
title: Native networks
description: "Create a Tako network, enroll servers and devices, deploy over pinned HTTPS from a sandbox, and understand private routing and revocation."
---

A native network identifies trusted devices and servers without an external account. Each device generates its own keys. A project selects the network by its stable 32-character hexadecimal ID; local aliases can differ between laptops.

Management uses signed requests over pinned HTTPS. Deploy, status, logs, releases, and scaling do not require a workstation WireGuard interface, TUN device, or SSH agent. Server installation still needs administrator SSH access. Tako networking is the supported enrollment path; see [re-enrollment](/servers/#choose-the-vpn-provider) for older installations.

## Create a network and enroll a server

Run these commands on the administrator's machine:

```bash
tako network create production
tako network list --json
tako servers add admin@203.0.113.10 --network production --name edge-a --wireguard-endpoint 203.0.113.10:51820
tako network status production --json
```

Replace the example IP with the server's reachable IP. Native servers require Linux with glibc on a supported architecture. Installation generates the server's keys on that server and records its endpoint and HTTPS certificate pins in network membership. It does not copy the administrator's private keys to the server or add a legacy `[[servers]]` entry.

Allow the configured HTTPS TCP port (default `443`) for management and the configured WireGuard UDP port (default `51820`) for private network traffic. The advertised endpoint must be reachable by peers. There is no hosted relay or automatic NAT traversal service; HTTPS management reachability alone does not establish WireGuard connectivity.

Stopping the native network agent withdraws its WireGuard peers and SSH grants. On startup the agent reapplies current membership before restoring access. If a previous failed enrollment reports `foreign interface`, inspect the existing WireGuard interface before retrying. Rebooting clears a transient interface left by an earlier installation.

A public listener configured with mandatory `trusted_proxy.proxy_protocol` cannot also accept direct native pinned HTTPS. Remove that mandatory listener setting before native enrollment; the server refuses to start with both enabled. Ordinary Cloudflare HTTP forwarding headers remain supported without PROXY protocol. Review the listener configuration before bootstrap so enrollment does not leave a server awaiting an incompatible restart.

Copy the stable ID from `network list` into your project:

```toml
[envs.production]
network = "0123456789abcdef0123456789abcdef"
route = "app.example.com"
```

Omitting `servers` deploys to every trusted server in the network. To place an app on specific hosts, list their enrolled names or device IDs in that app’s project:

```toml
[envs.production]
network = "0123456789abcdef0123456789abcdef"
servers = ["edge-a", "edge-b"]
route = "shop.example.com"
```

A billing app on the same network can set `servers = ["billing-c"]` in its own project. Both apps can still reach each other through their private service names. The CLI resolves the entire list against trusted membership before deploying. An empty list, an unknown server, or an ambiguous name stops the deploy. Use the server’s device ID from `tako network status` to distinguish duplicate names. Removing a host from the list changes future deploys. Use `tako delete` on that host to remove an existing release.

```bash
tako deploy --env production
tako status --json
tako logs --env production --days 1 --json
```

Check the command exit status, membership, and application response separately. A successful dry run or upload does not verify a deployed application. Compose environments still require exactly one target server. Outside a project, `tako status` lists every configured network; one whose membership cannot be refreshed is reported as a warning and skipped instead of failing the command.

## Enroll another laptop or sandbox

Generate a request on the new device, using the network's stable ID:

```bash
tako network request --name laptop-b --network 0123456789abcdef0123456789abcdef --output laptop-b.request.json
```

Transfer that public request to an administrator through a trusted channel. On the administrator's machine, grant only the required app/environment permissions:

```bash
tako network approve laptop-b.request.json --network production --deploy shop/production --expires-in 3600 --output laptop-b.profile.json
```

Repeat `--deploy app/env` for additional scopes. `--expires-in` sets membership lifetime in seconds, from `1` through `31536000`; omitting it grants membership without that expiry. The enrollment request and pending approval have a separate, bounded activation window. Generate a new request if enrollment expires.

For a full administrator, use `--admin`; add `--ssh` only when that device also needs a Tako administrator shell. Deployment scope does not grant network administration, server maintenance, or shell access. It limits management actions; application authentication remains separate from network membership.

Return the approved profile to the requesting device:

```bash
tako network import laptop-b.profile.json --name production
tako network status production --json
tako deploy --env production --ci
```

Import requires the keys generated by that device's request. Refresh activates a pending approval over HTTPS and proves possession of those keys. Do not copy an existing laptop's key directory to enroll another device.

A scoped sandbox deploy uses routes and server-level credential/storage bindings already provisioned by an administrator. The server checks the exact app/environment scope and rejects unauthorized route or binding changes. Provision these before granting access; a scoped deploy cannot create global ingress configuration. The sandbox needs outbound access to the servers' HTTPS endpoints and its usual build dependencies, but no local VPN setup.

If an import loses its successful response, retry the import or connection. After the approval expires, Tako can recover membership that the server already admitted. The device must still have valid membership. Expired approvals cannot enroll new devices.

## Keep network identity and app secrets separate

```bash
tako network export production --output production.public.json
```

Network export contains public membership and server trust information. It contains neither private device keys nor app secrets, and it cannot enroll a different device by itself. Use request, approve, and import for a new device.

[Secret-key export and import](/cli/#secrets) are separate, explicitly authorized per-environment operations. Network membership does not decrypt the project's encrypted secret store. Give a deployment device the required environment secret access through your existing secret-sharing process; do not export unrelated environments or replace working keys to fix an access error.

## Revoke a device

Inspect device IDs before revoking:

```bash
tako network status production --json
tako network revoke DEVICE_ID --network production
tako network status production --json
```

Revocation invalidates the device and approvals derived from it. Descendants cannot retain authority through an expired or revoked issuer. The first server bootstrapped from a newly created network receives independent administrator membership, so revoking the creating laptop does not remove that server solely through the laptop's delegation chain. Later delegated memberships remain subject to their issuer chains.

Revocations merge monotonically: importing or refreshing an older snapshot cannot restore a revoked identity. Offline servers still need to receive the updated state before they enforce it. A revoke response can report enforcement as pending; inspect refreshed membership and enforcement acknowledgments rather than assuming every peer has already applied the change. If the response is lost, repeat the same revoke command on the same device. Tako keeps the original request ID and issuer so the retry cannot become a different revocation, even after that device revokes itself. An unreachable issuer leaves the attempt pending.

## SSH without administrator access

```bash
tako ssh edge-a --network production
tako ssh edge-a --network production -- uname -a
```

The device needs an approved SSH permission (`--admin --ssh` during approval). Tako uses that device's SSH identity, the approved username, and the server's pinned host key. Public TCP port 22 can stay closed; the server's WireGuard UDP endpoint must be reachable and its private SSH listener must be running.

Tako embeds BoringTun and a userspace TCP stack. `tako ssh` needs OpenSSH but no WireGuard executable, sudo, TUN device, system routes, or DNS changes. Concurrent sessions to the same server share an SSH transport. If `tako connect` is already active, SSH uses that system connection.

## Connect local applications

```bash
tako connect production
ssh edge-a
psql -h edge-a -p 5432 -U app app
tako disconnect production
```

`connect` runs the embedded BoringTun engine in a background Tako process and configures private routes, server names, and private service DNS. It supports macOS and Linux. Network setup requires administrator access; Tako explains the reason before asking for the local sudo password. Linux also needs `/dev/net/tun`, `iproute2`, and an active systemd-resolved resolver with `resolvectl`. macOS uses its native tunnel interface and scoped resolver files. Neither platform needs `wg-quick`, `wg`, or `wireguard-go` installed.

While connected, ordinary applications can resolve server names such as `edge-a` and `edge-a.NETWORK_ID.tako.internal`. Tako adds managed entries to `/etc/hosts` and, for devices with SSH permission, a managed block in `~/.ssh/config` with the approved identity, username, and host-key pin. Conflicting existing aliases produce an error instead of being overwritten. A database must listen on its private address and allow the connection; network membership does not replace database authentication or expose a container-only port.

`disconnect` removes Tako's routes, resolver settings, host entries, and SSH aliases while preserving unrelated settings. Server peers, names, and SSH settings are refreshed by each `connect`; rerun it after membership changes. Finish active `tako ssh` sessions before changing the system connection. If upgrading from the old external WireGuard implementation with a connection still active, disconnect it using the previous Tako version first.

## Use private service addresses

Private app hostnames have this form:

```text
shop.production.0123456789abcdef0123456789abcdef.tako.internal
```

The components are app name, environment, and stable network ID. Native service discovery uses current authenticated placement information. Missing, expired, revoked, or drained placements are unavailable; a private name must not fall through to a public wildcard route. Supported Compose services use Tako's private gateway and resolver for this traffic.

Automatic private DNS for native processes on the server requires an active systemd-resolved service. If it is absent, the WireGuard helper can still configure the link but reports `dns_supported = false` with a DNS error; do not treat link setup as successful DNS setup. Compose uses its managed gateway DNS independently of host systemd-resolved.

While `tako connect` is active, the workstation sends queries for this network's private domain to its servers over WireGuard. Public DNS and the default route stay unchanged. Applications using their own DNS resolver instead of the operating system may need separate configuration. A scoped HTTPS deployment does not need a local connection. Ordinary HTTP clients still need a reachable URL. [Network primitive calls](/network-primitives/) use `app.primitive` addresses and resolve placement through Tako, without a user-configured base URL.

Public application DNS, certificates, and ingress routes remain separate. A native network does not move your public hostname between servers or configure an external load balancer.

## Understand replay and maintenance limits

Native servers can replicate bounded channel replay and fan out events over their private transport. Presence uses expiring, server-bound leases. These are live-delivery mechanisms, not permanent application history. When replay is unavailable, the JavaScript SDK handles protocol resets quietly and application code reloads authoritative state. See [Channels](/channels/#deploy-replay-storage) for replay storage and recovery requirements. Native channels, presence, and store notifications can use SQLite across servers without Postgres; a configured Postgres credential is preserved and validated.

Channel replication does not migrate durable workflow executions, databases, or volumes. [Workflow storage](/workflows/) and [Compose volumes](/docker-compose/) retain their own persistence requirements.

```bash
tako network drain edge-a --network production --timeout 120 --json
tako network resume edge-a --network production --json
```

Server drain records maintenance intent and rejects new app traffic while retaining management and network control. `status = "draining"` means cleanup is still pending. `completed = true` requires every proxy process on that server to be quiescent. Inspect `timed_out`, remaining work, forced work, and errors; request counts describe the process handling the command and can overlap instance counts. Forced or incomplete cleanup is not graceful completion. Resume restores persisted apps before reopening traffic. Drain does not move local volumes, migrate workflows to another server, or redirect global ingress; plan those operations separately. See [Operations](/operations/) for maintenance guidance.
