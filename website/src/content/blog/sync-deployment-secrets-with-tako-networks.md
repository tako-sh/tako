---
title: "Sync Deployment Secrets Across Machines with Tako Networks"
date: "2026-11-14T05:18"
description: "Share server membership and encrypted secrets through Tako networks, onboard a new machine with an invitation, and keep worktree drafts out of Git."
image: 10fc19781a3f
imageAlt: "A coral octopus connects a laptop to a locked server while a newcomer presents an invitation."
---

Cloning an app should give you its code and deployment configuration. Getting permission to deploy it should give you access to the servers and secrets it needs. We have moved Tako's shared operational state into the network, so those two steps fit together: clone the repository, redeem an invitation, and sync. `tako.toml` stays in Git; server inventory and published encrypted secrets live on the network's servers.

## Give each kind of state a home

Previously, Tako kept server inventory in `servers.toml` and encrypted app secrets in a tracked `.tako/secrets.json`. Setting up another machine involved reconstructing the inventory and transferring environment keys. Encryption protected the values, but Git was still carrying operational state alongside code.

The new arrangement gives each piece a clear home:

| State                                                 | Where it lives                                 | How it moves                        |
| ----------------------------------------------------- | ---------------------------------------------- | ----------------------------------- |
| App code, build settings, routes, environment targets | Git, including `tako.toml`                     | Branches, reviews, and merges       |
| Network name and server/client membership             | Network servers, with local client caches      | Enrollment and network sync         |
| Published encrypted secrets and credential records    | Network servers, with authorized client caches | Secret publication and network sync |
| Unpublished secret edits                              | Ignored state in each worktree                 | Explicit publication or deploy      |
| Private device keys                                   | That device's local Tako state                 | Each device generates its own       |

Keeping [app configuration in `tako.toml`](/docs/tako-toml/) matters when several agents work in parallel. A branch changing a build command or route should carry that change through review. Moving those settings into a shared mutable database would make the branch incomplete: checking out its code would no longer tell you how it is meant to run.

Server membership is different. A revoked laptop should stay revoked regardless of which Git branch somebody checks out. A newly enrolled server should be discoverable without copying another configuration file. Membership now supplies the inventory used by `tako network server` commands.

We kept [Corrosion](https://github.com/superfly/corrosion) for server replication. It already provides eventually consistent state across Tako servers. Clients keep a local cache and make signed requests over pinned HTTPS when a command needs to sync; they do not run Corrosion or connect to other clients.

```d2
direction: right

laptop: "Developer laptop\ncache + local drafts"
sandbox: "Agent sandbox\ncache + local drafts"
server_a: "Network server A"
server_b: "Network server B"

laptop -> server_a: "Pinned HTTPS"
sandbox -> server_b: "Pinned HTTPS"
server_a <-> server_b: "Corrosion replication"
```

There is no background client connection to maintain. Offline edits work locally; publishing or fetching another machine's changes needs a reachable server. Replication also takes time, so a disconnected server can lag. This state system does not replicate your application's database or volumes, and it does not provide a global deployment lock.

## Bring a new machine onto the network

On an administrator's machine, a fresh setup starts with a named network and an enrolled server:

```bash
tako network create production
tako network server add admin@203.0.113.10 --network production --name edge-a --wireguard-endpoint 203.0.113.10:51820
tako network list --json
```

Replace the example address with your server's reachable IP. Installation still uses administrator SSH access. Subsequent deployment management uses pinned HTTPS, so a laptop or sandbox does not need a local VPN interface to deploy. Servers still need their private WireGuard connectivity; the [network setup guide](/docs/networks/) covers ports and host requirements.

The network name is shared across enrolled machines. Its stable ID goes into the project's environment configuration, copied from `network list --json`. For an app named `shop`, the relevant excerpt is:

```toml
name = "shop"

[envs.production]
network = "0123456789abcdef0123456789abcdef"
servers = ["edge-a"]
route = "shop.example.com"
```

Use your own network ID and hostname, keeping the rest of your app configuration. The `servers` list selects placement; omitting it targets every trusted server in that network. It is not another server registry.

An administrator can create an invitation for another administrator:

```bash
tako network client add --network production --admin
```

For a deployment agent, grant a specific app and environment instead:

```bash
tako network client add --network production --deploy shop/production
```

That scoped client can access the app secrets for `shop/production`. It uses routes and server credential bindings an administrator has already provisioned, so arrange those before its first deployment. Administrators and trusted servers can decrypt the broader shared secret categories; deployment clients do not receive that authority automatically.

Transfer the invitation privately. On the new machine, clone your app repository, enter it, then run:

```bash
tako network join --name laptop-b
tako network sync production
tako secrets list --json
```

`join` prompts for the invitation; automation can use `--invitation-stdin`. The invitation is single-use and expires after an hour by default. Its issuing server must be reachable to redeem it. The new device generates its own keys: the string grants enrollment, rather than copying an existing client's identity. Secret listing shows metadata without printing values.

An invitation recovers access to published state. It cannot recover edits somebody never published. Keep backups of server state and administrator keys, and review the [revocation rules](/docs/networks/#revoke-a-device) before removing an administrator: delegated clients depend on their inviter's authority. Revocation also cannot erase secrets a device already copied.

## Publish secret edits deliberately

From the app directory, set a value using the masked prompt:

```bash
tako secrets set DATABASE_URL --env production
```

This writes encrypted, ignored `.tako/secrets.local.json` state in the current worktree. It makes no network request. Removing a secret is local too. For automation, pass values on stdin; avoid putting them into shell arguments or history.

Publication happens when you choose it:

| Command                              | Shared-state effect                                          | Running app effect                               |
| ------------------------------------ | ------------------------------------------------------------ | ------------------------------------------------ |
| `tako network sync production`       | Refreshes the device cache; leaves drafts unpublished        | None                                             |
| `tako secrets sync --env production` | Pulls published values and publishes this worktree's changes | None                                             |
| `tako deploy --env production`       | Syncs the selected environment before building               | Deploys a new release if the deployment succeeds |

Adding or removing a client refreshes membership and the local cache without publishing unrelated drafts. Secret sync alone does not restart apps. Also, because deployment publishes before building, a later build failure does not undo secret publication. See the [deployment flow](/docs/deployment/) when planning a rotation.

Changes merge per key. If one machine edits `DATABASE_URL` and another edits `PAYMENT_TOKEN`, both changes can survive. Concurrent edits to the same key resolve deterministically using logical revisions and stable tie breakers, independent of laptop wall clocks. Deletions publish tombstones so stale caches cannot bring deleted values back. Storage access and secret keys travel together as one credential record.

A lost publication response leaves a durable request to retry. An unconfirmed write may need its original server to return; switching servers does not make it a fresh edit that wins again. This still is not an atomic rotation of several secrets. Coordinate dependent changes and simultaneous deployments to the same environment.

Worktrees deserve particular attention. A new worktree can start from the machine's published cache, while existing worktrees keep their own drafts. Merging a branch does not carry those drafts into the release checkout. Set required values there, or publish them explicitly before deploying the merged code. Deploying a branch still uses its configured environment; it does not automatically create a branch preview.

For an existing tracked secret file, follow the [migration instructions](/docs/secrets/#migrate-a-tracked-secret-file):

```bash
tako secrets migrate
tako secrets sync --env production
```

Migration requires the original environment keys, retains an ignored recovery copy, and removes the old working file. Review and commit that deletion. Previous Git commits still contain their encrypted blobs. Older native network identities also require fresh enrollment for this protocol update; secret migration does not upgrade those identities.

Shared state now retains only the current encrypted value per key, with versioned deletion markers and a seven-day retry window for interrupted publications. Backup encryption keys remain separate records by key ID. If every administrator laptop is lost, a surviving server can issue an independent administrator invitation with `sudo tako-server admin invite`. See the [secret storage limits](/docs/secrets/#apply-changes-to-deployed-apps) and [administrator recovery procedure](/docs/networks/#recover-administrator-access). Recovery requires intact server state and keys; shared state still needs backups.

The everyday workflow is smaller: review code and `tako.toml` in Git, enroll machines through invitations, and publish secret edits when they are ready to be shared.
