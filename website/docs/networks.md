---
title: Native networks
description: "Create a Tako network, enroll servers and devices, deploy over pinned HTTPS from a sandbox, and understand private routing and revocation."
---

A native network identifies trusted devices and servers without an external account. Each device generates its own keys. Every network has a stable 32-character hexadecimal ID and a name shared by every member. The name is set when the network is created, can't change, and may repeat across networks. Each machine also gives every network it belongs to a local [alias](#name-networks-on-this-machine), unique on that machine. Commands take a network by alias or ID, never by its shared name.

A project stores each environment's network ID in `[envs.<env>].network` in `tako.toml`, which you commit to Git. Inside a project, pass `--env` and Tako finds the network; see [Choose an environment's network](#choose-an-environments-network).

Network commands such as `network status`, `network client add`, `network server add`, `connect`, and `ssh` act on the network passed as an argument or with the global `--network` flag. Without one, they use this machine's only network. With several, an interactive terminal asks which one; without a terminal the command fails and lists this machine's aliases. There is no default network. [Server maintenance selectors](/servers/#server-maintenance) name their network by alias, such as `production/edge-a`.

Management uses signed requests over pinned HTTPS. Deploy, status, logs, releases, and scaling do not require a workstation WireGuard interface, TUN device, or SSH agent. Server installation still needs root or sudo SSH access to the server. Tako networking is the supported enrollment path; see [re-enrollment](/servers/#choose-the-vpn-provider) for older installations.

This protocol-v0 update changes the enrollment format to bind a separate secret-encryption key. Older native identities and profiles require a coordinated upgrade and fresh enrollment; ordinary server upgrade does not migrate them in place. Preserve application data, old network state, and legacy environment keys before rebuilding an existing network. `secret migrate` preserves app secret values and keys, but does not convert old network identities.

## Create a network and enroll a server

Run these commands on your machine:

```bash
tako network create production
tako network list --json
tako network server add admin@203.0.113.10 --network production --name edge-a --wireguard-endpoint 203.0.113.10:51820
tako network status production --json
```

Replace the example IP with the server's reachable IP. Native servers require Linux with glibc on a supported architecture. Installation generates the server's keys on that server and records its endpoint and HTTPS certificate pins in network membership. It does not copy the administrator's private keys to the server or add a legacy `[[servers]]` entry.

Allow the configured HTTPS TCP port (default `443`) for management and the configured WireGuard UDP port (default `51820`) for private network traffic. The advertised endpoint must be reachable by peers. There is no hosted relay or automatic NAT traversal service; HTTPS management reachability alone does not establish WireGuard connectivity.

Stopping the native network agent withdraws its WireGuard peers and SSH grants. On startup the agent reapplies current membership before restoring access. If a previous failed enrollment reports `foreign interface`, inspect the existing WireGuard interface before retrying. Rebooting clears a transient interface left by an earlier installation.

A public listener configured with mandatory `trusted_proxy.proxy_protocol` cannot also accept direct native pinned HTTPS. Remove that mandatory listener setting before native enrollment; the server refuses to start with both enabled. Ordinary Cloudflare HTTP forwarding headers remain supported without PROXY protocol. Review the listener configuration before bootstrap so enrollment does not leave a server awaiting an incompatible restart.

Set the network ID from `tako network list --json` in `tako.toml`. If omitted, the first `tako deploy --env production` uses this machine's only network, or asks which one, and records its ID in the same table:

```toml
[envs.production]
network = "0123456789abcdef0123456789abcdef"
route = "app.example.com"
```

Omitting `servers` deploys to every trusted server in the environment's network. To place an app on specific hosts, list their enrolled names or device IDs in that app’s project:

```toml
[envs.production]
servers = ["edge-a", "edge-b"]
route = "shop.example.com"
```

A billing app on the same network can set `servers = ["billing-c"]` in its own project. Both apps can still reach each other through their private service names. The CLI resolves the entire list against trusted membership before deploying. An empty list, an unknown server, or an ambiguous name stops the deploy. Use the server’s device ID from `tako network status` to distinguish duplicate names. Removing a host from the list changes future deploys. Use `tako delete` on that host to remove an existing release.

```bash
tako deploy --env production
tako status --json
tako logs --env production --days 1 --json
```

Check the command exit status, membership, and application response separately. A successful dry run or upload does not verify a deployed application. Compose environments still require exactly one target server. With several networks, `tako status` in an interactive terminal (without `--json`) asks for all networks or one; `--network` narrows it to one, and without a terminal it shows every network. When it shows every network, one whose membership cannot be refreshed is reported as a warning and skipped instead of failing the command.

Deploy resolves membership and cached secret values for the selected environment's network. An expired membership in an unrelated network does not block that deploy. Invalid membership in the selected network remains an error.

## Choose an environment's network

`tako.toml` stores the network each environment deploys to, by ID:

```toml
[envs.production]
network = "0123456789abcdef0123456789abcdef"
route = "app.example.com"
```

Set the ID yourself or let Tako record it on first deploy. Commit the config so every clone deploys each environment to the same network. Recording preserves comments and formatting and honors `--config`; alternate config files have their own environment networks. `.tako/`, including `state.local.json`, stays local and ignored.

A project command picks the environment's network in this order:

1. The global `--network <alias-or-id>`.
2. `[envs.<env>].network` in the selected TOML config.
3. This machine's only network.
4. In an interactive terminal, a `Network for <env>` question. Without a terminal, the command fails, lists this machine's aliases, and asks for `--network`.

`tako deploy` puts an environment on a network, so it records the network it used and prints a hint to commit the config. When `--network` names a different network than the configured one, it asks to move the environment (the default answer is no) and then updates `network`; without a terminal it fails. A move is refused before anything is recorded while this checkout has unpublished secret drafts for the old network. `tako secret set`, `rm`, and `sync` record the network only when the environment has none yet; with a different `--network` they act on that network for that run, which publishes drafts written for another network. Other commands, such as `logs`, `release list`, `release rollback`, `start`, `stop`, `scale`, `delete`, `backup`, `workflow`, and `secret clear`, use `--network` for that run only and leave the config unchanged.

A recorded network that this machine hasn't joined fails the command with a hint to run `tako network join` or pass `--network`. The `development` environment runs locally with `tako dev` and never has a network. In CI, `TAKO_CLIENT_IDENTITY` holds exactly one network, so commands use it without flags; a network recorded for the environment must be that one.

Moving an environment does not copy its secrets. See [Secrets](/secrets/#move-an-environment-to-another-network) before moving one.

## Name networks on this machine

Each machine names every network it belongs to by its own alias. Commands accept the alias or the stable ID wherever they take a network. Aliases follow the network-name rules: they start with a lowercase letter, use only lowercase letters, digits, and `-`, are at most 63 characters, and can't look like a network ID. An alias is unique on its machine; other machines can call the same network something else.

```bash
tako network create production --alias prod-eu
tako network join --alias acme-prod
tako network rename prod-eu europe
tako network list --json
```

- `network create <name>` uses the name as the alias. When this machine already has a network with that alias, an interactive terminal asks for another, suggesting `<name>-2`; without a terminal the command fails and asks for `--alias`.
- `network join` asks for the alias in a terminal, suggesting the network's name, or `<name>-2` when that alias is taken here. `--alias` skips the question. Without a terminal, join uses the name when it is free and otherwise fails and asks for `--alias`. It never picks `<name>-2` on its own, so a script's next `--network <name>` can't reach an older network. Joining a network this machine already has keeps its alias.
- `network rename <alias-or-id> <new-alias>` changes only this machine's alias. Other members keep theirs, and the shared name stays the same.
- `network list` prints each alias, with the shared name in parentheses when the two differ, such as `prod-eu (production)`. `--json` returns `{"networks": [{"alias", "name", "id", "device"}]}`.

`tako.toml` records networks by ID, so aliases never reach committed files.

## Enroll another laptop or sandbox

Create a short-lived, single-use invitation on an authorized machine:

```bash
tako network client add laptop-b --network production
# Or restrict a client to deploying a specific app/environment:
tako network client add shop-ci --network production --deploy shop/production
```

Transfer the invitation privately. On the new machine, clone the app repository and run:

```bash
tako network join
tako network sync production
tako deploy --env production --ci
```

The clone brings `tako.toml`, so `tako deploy --env production` uses the network configured for production, whatever this machine calls it. `join` prompts for the invitation and this machine's [alias](#name-networks-on-this-machine) for the network; automation can supply the invitation on stdin with `--invitation-stdin` and the alias with `--alias`. Each joining device generates its own signing, WireGuard, and encryption keys. An invitation is a one-use enrollment credential, not a reusable device key. It expires after one hour by default; `client add --expires-in` accepts 60–86400 seconds. The issuing server must be reachable to redeem it. Retrying the same join on the same machine can recover a lost response; another machine cannot reuse the invitation.

A client has one of two access levels. A **full** client, the default, can do everything in the network: decrypt shared app secrets, provider credentials, storage credentials, and backup keys, deploy any app, invite and revoke clients, add servers, open the `tako` shell, and join the private network with `tako connect`. A **deploy** client, created with one or more `--deploy app/env` scopes, can only release code to those environments: deploy, run the release command, roll back, and read status and releases. It cannot read or change secrets or credentials, read logs, scale, stop, delete, or manage backups and workflows. It manages over pinned HTTPS and never becomes a WireGuard peer, so `tako connect` and `tako ssh` refuse it. Treat a network as the trust boundary: put people or environments you do not fully trust on a separate network. Network access does not replace application authentication.

Clients contact servers only. Servers replicate membership and encrypted values with Corrosion; clients do not run Corrosion or connect to each other. A join and membership changes refresh the local cache. `network sync` pulls the network state without publishing worktree drafts. `deploy` and `secret sync` also publish that worktree's changes for the selected environment. No background client connection is required.

For a machine that keeps no state between runs, such as a CI runner, create a client identity instead of an invitation:

```bash
tako network client add shop-ci --network production --deploy shop/production --identity
```

The identity is the new client's keys and network profile in one string, printed on stdout so it can go straight into a secret store. Set it as `TAKO_CLIENT_IDENTITY` and `tako deploy` acts as that client; the keys stay in a private temporary directory for the command and never reach the Tako home. `tako network join` also accepts an identity and stores it like a joined device. Anyone holding the identity is that client until you run `tako network client revoke shop-ci`, so keep it in a protected secret and give it `--deploy` scopes. Without `--deploy`, the identity has full access and `client add` warns about it.

A deploy client needs no local secrets or keys. It sends no secret values: servers apply the environment's values that full clients published. It can make the first deploy of its app/environment and change its routes, SSL provider, and storage and backup settings in `tako.toml`; the servers fill in credentials and keys. Database bindings need a full client, which grants database access. Every server rejects a route that another app or environment already claims, for every client. A deploy client also can't add a route that overlaps another environment's route, such as `shop.example.com` under production's `*.example.com` or `example.com/api/*` under production's `example.com`, since the more specific route would take those requests; routes its environment already has may stay. Split a domain between environments with a full client. The sandbox needs outbound access to the servers' HTTPS endpoints and its usual build dependencies, but no local VPN setup.

If a join loses its successful response, run the join again on the same machine. After the invitation expires, Tako can recover membership that the server already admitted. The device must still have valid membership. Expired approvals cannot enroll new devices.

## Keep network identity and app secrets separate

```bash
tako network export production --output production.public.json
```

Network export contains public membership and server trust information. It contains neither private device keys nor app secrets, and it cannot enroll a different device by itself. Use an invitation to enroll a new device.

Network membership grants access according to the device's access level; published secrets are encrypted to authorized device keys. Keep `tako.toml`, including each environment's network ID, in Git. Keep `.tako/` local and ignored. An invitation restores access to the network state, but cannot recover another machine's unpublished drafts. Back up server state and the keys of at least one full client.

See [Secrets](/secrets/) for worktree isolation, publication, and migration from a tracked secret file. Legacy environment-key import/export remains available for migration and explicit recovery; it is no longer the normal new-device workflow.

## Recover access

If the only full client's laptop is lost, open a surviving server through the provider's web console or the server's SSH and run:

```bash
sudo tako-server client invite
```

On the replacement machine, run `tako network join` and paste the invitation at the prompt. The string includes the issuing server's IP and ports, certificate trust information, network name and ID, a single-use token, and its expiry. The replacement generates its own private keys. The issuing server must remain reachable until the join completes.

The invitation creates a full client. It defaults to one hour; `--expires-in` accepts 60–86400 seconds, and `--json` prints structured output. The token is sensitive: give it only to the intended person. Only root on the server can issue recovery invitations; this command is unavailable through the public management API or the unprivileged `tako` account.

Verify access with `tako network sync NETWORK`, list the clients, then revoke the lost device's client with `tako network client revoke CLIENT --network NETWORK`, where `NETWORK` is the alias you chose at join. The recovered client has no inviter, so nothing about the lost device affects it.

Recovery requires an active server with its network state, private keys, and network agent intact. A server whose membership is revoked cannot issue recovery invitations. This restores access to surviving published state; it cannot reconstruct unpublished laptop drafts or a network whose every copy and key has been lost. App-data backups do not include the network registry or device keys.

## Revoke a device

Inspect clients before revoking:

```bash
tako network status production --json
tako network client list --network production
tako network client revoke CLIENT --network production
tako network status production --json
```

Revocation affects only the revoked device. Clients it invited keep their access, because an invitation stays valid once the inviter approved it while still valid; an approval recorded after the inviter's revocation grants nothing. Someone holding a stolen full device can invite a client before you revoke the device, and that client survives the revocation. To cut off everything a device invited, directly or through the clients it invited, add `--include-invited`: `tako network client revoke CLIENT --include-invited --network production`. A device cannot revoke itself, and the last full client cannot be revoked; use [host recovery](#recover-access) when no full client remains.

Revocations merge monotonically: importing or refreshing an older snapshot cannot restore a revoked identity. Offline servers still need to receive the updated state before they enforce it. Revocation prevents access to future secret values after enforcement; it cannot erase plaintext or keys the device already copied. A revoke response can report enforcement as pending; inspect refreshed membership and enforcement acknowledgments rather than assuming every peer has already applied the change. If the response is lost, repeat the same revoke command on the same device. Tako keeps the original request ID and issuer so the retry cannot become a different revocation. An unreachable issuer leaves the attempt pending.

## SSH without a public port

```bash
tako ssh edge-a --network production
tako ssh edge-a --network production -- uname -a
```

Full clients have SSH access to the `tako` account; deploy clients have none. Tako uses the device's SSH identity and the server's pinned host key. Public TCP port 22 can stay closed; the server's WireGuard UDP endpoint must be reachable and its private SSH listener must be running.

Tako embeds BoringTun and a userspace TCP stack. `tako ssh` needs OpenSSH but no WireGuard executable, sudo, TUN device, system routes, or DNS changes. Concurrent sessions to the same server share an SSH transport. If `tako connect` is already active, SSH uses that system connection.

If the server never answers the WireGuard handshake, `tako ssh` stops after 10 seconds with `UDP 51820 to 203.0.113.10 isn't getting through` instead of waiting on OpenSSH. A firewall that drops UDP and a server that does not yet know this device look the same from outside, so check the server's firewall and provider security group first, then membership. A refused port means nothing is listening there or a firewall is rejecting it. If the handshake succeeds but SSH does not answer, the error says so. `tako doctor` runs the same handshake check against every server in every local network.

### Use scp, rsync, and editors

```bash
tako connect production
scp backup.tar edge-a:
rsync -a ./data/ edge-a:data/
ssh -L 5432:localhost:5432 edge-a
```

While [connected](#connect-local-applications), the SSH aliases `edge-a` and `edge-a.tako` work with any OpenSSH-based tool, including scp, rsync, sftp, local port forwarding, and VS Code Remote-SSH, without public port 22. Without a connection, use `tako ssh` for a shell or a single command.

Network SSH keys allow local forwarding (`ssh -L`, `ssh -D`) only to the server's own loopback addresses, such as a database bound to `127.0.0.1`. That is no more than the granted shell can already reach. Remote forwarding (`-R`), agent forwarding, and X11 stay disabled. Servers on earlier releases refuse all forwarding until upgraded. Servers running OpenSSH older than 7.8, and servers whose sshd sets `AllowTcpForwarding no`, keep refusing it.

## Connect local applications

```bash
tako connect production
ssh edge-a
psql -h edge-a -p 5432 -U app app
curl http://shop.production.tako/
tako disconnect
```

`connect` runs the embedded BoringTun engine in a background Tako process and configures private routes, server names, and private service DNS. It supports macOS and Linux. Network setup requires administrator access; Tako explains the reason before asking for the local sudo password. Linux also needs `/dev/net/tun`, `iproute2`, and an active systemd-resolved resolver with `resolvectl`. macOS uses its native tunnel interface and scoped resolver files. Neither platform needs `wg-quick`, `wg`, or `wireguard-go` installed.

While connected, ordinary applications can resolve server names such as `edge-a` and `edge-a.tako`. Tako adds managed entries to `/etc/hosts` and, for devices with SSH permission, a managed block in `~/.ssh/config` with the approved identity, username, and host-key pin. Conflicting existing aliases produce an error instead of being overwritten. A database must listen on its private address and allow the connection; network membership does not replace database authentication or expose a container-only port.

After connecting, Tako waits a few seconds for a WireGuard handshake with each server and names any server that did not answer, with the UDP endpoint to check. Management keeps working over HTTPS even when that UDP port is blocked.

One network owns `.tako` names on a workstation at a time, so `connect` disconnects any other connected network first. `disconnect` removes Tako's routes, resolver settings, host entries, and SSH aliases while preserving unrelated settings. Without a network argument it disconnects whichever network is connected. Server peers, names, and SSH settings are refreshed by each `connect`; rerun it after membership changes. Finish active `tako ssh` sessions before changing the system connection. If upgrading from the old external WireGuard implementation with a connection still active, disconnect it using the previous Tako version first.

## Use private service addresses

Private names live under `.tako`:

```text
shop.production.tako    # app "shop", environment "production"
edge-a.tako             # server "edge-a"
```

App names have two labels: app and environment. Server names have one label, the lowercased server name. Active servers in a network must have unique names; enrollment rejects a duplicate. Names that are not a single DNS label, such as names containing dots, are not published. Native service discovery uses current authenticated placement information. Missing, expired, revoked, or drained placements are unavailable; a private name must not fall through to a public wildcard route. Placements are short-lived and signed by the server that runs the app, so keep server clocks synchronized: peers accept a placement signed up to five seconds ahead of their own clock and ignore one further ahead until their clock catches up. Supported Compose services use Tako's private gateway and resolver for this traffic.

Automatic private DNS for native processes on the server requires an active systemd-resolved service. If it is absent, the WireGuard helper can still configure the link but reports `dns_supported = false` with a DNS error; do not treat link setup as successful DNS setup. Compose uses its managed gateway DNS independently of host systemd-resolved.

While `tako connect` is active, the workstation sends `.tako` queries to the connected network's servers over WireGuard. Each server belongs to one network, so its `.tako` names are unambiguous. Browsers treat bare `.tako` names as searches; type `http://` or a trailing `/` the first time. Public DNS and the default route stay unchanged. Applications using their own DNS resolver instead of the operating system may need separate configuration. A scoped HTTPS deployment does not need a local connection. Ordinary HTTP clients still need a reachable URL. [Network primitive calls](/network-primitives/) use `app.primitive` addresses and resolve placement through Tako, without a user-configured base URL.

Public application DNS, certificates, and ingress routes remain separate. A native network does not move your public hostname between servers or configure an external load balancer.

## Understand replay and maintenance limits

Native servers can replicate bounded channel replay and fan out events over their private transport. Presence uses expiring, server-bound leases. These are live-delivery mechanisms, not permanent application history. When replay is unavailable, the JavaScript SDK handles protocol resets quietly and application code reloads authoritative state. See [Channels](/channels/#deploy-replay-storage) for replay storage and recovery requirements. Durable channel replay, presence, and store notifications can use SQLite across servers without Postgres; a configured Postgres credential is preserved and validated. Ephemeral channels use memory-only delivery without replay.

Channel replication does not migrate durable workflow executions, databases, or volumes. [Workflow storage](/workflows/) and [Compose volumes](/docker-compose/) retain their own persistence requirements.

```bash
tako network server drain edge-a --network production --timeout 120 --json
tako network server resume edge-a --network production --json
```

Server drain records maintenance intent and rejects new app traffic while retaining management and network control. `status = "draining"` means cleanup is still pending. `completed = true` requires every proxy process on that server to be quiescent. Inspect `timed_out`, remaining work, forced work, and errors; request counts describe the process handling the command and can overlap instance counts. Forced or incomplete cleanup is not graceful completion. Resume restores persisted apps before reopening traffic. Drain does not move local volumes, migrate workflows to another server, or redirect global ingress; plan those operations separately. See [Operations](/operations/) for maintenance guidance.
