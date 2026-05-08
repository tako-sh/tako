---
title: "Self-Hosted Deployments over Tailscale: Signed Remote Management for VPS Apps"
date: "2026-05-08T02:12"
description: "Tako keeps server management on private Tailscale HTTP and signs mutating RPCs with the SSH keys you already use for recovery."
image: eaa03cb44e6f
---

SSH is a wonderful recovery tool. It is not a wonderful status API.

For the first versions of Tako, the mental model was straightforward: the local [`tako` CLI](/docs/cli) talked to your server over SSH, and the server talked to its own Unix management socket. That works. It is also a little too honest about its ancestry. Every status check, secrets sync, deploy step, and future control-plane operation has to fit through an interactive login protocol that was designed for humans and shell commands.

The newer shape is cleaner: SSH gets you onto the box, enrolls the key, and stays available for setup and recovery. Normal management traffic moves to a private HTTP RPC endpoint on the server's Tailscale address. The endpoint is not public internet API glitter. It is a tiny, typed `Command -> Response` path bound to the tailnet, and every non-probe command is signed by an enrolled SSH key.

That gives self-hosted deployments the thing managed platforms usually hide from you: a control plane that is fast enough to use often, private by default, and still tied to keys you already understand.

## The management plane should be private

When you install `tako-server`, the server installer looks for the machine's Tailscale IP with `tailscale ip -4`. You can also pass `TAKO_MANAGEMENT_HOST` explicitly. In the normal service install path, Tako refuses to expose remote management unless that host is a Tailscale address.

That matters because the remote management listener is plain HTTP on port `9844`. Plain HTTP is fine when the only intended path is inside an encrypted tailnet, and it keeps the endpoint boring: no public certificate, no DNS challenge, no extra TLS stack for an API that should never be reachable from the open internet.

The installed server still serves your app on normal HTTP/HTTPS ports. Only management moves behind Tailscale:

| Traffic type       | Listener              | Who should reach it          | What it is for                                    |
| ------------------ | --------------------- | ---------------------------- | ------------------------------------------------- |
| Public app traffic | `:80` / `:443`        | Browsers and API clients     | Routes declared in [`tako.toml`](/docs/tako-toml) |
| Local server IPC   | Unix socket           | `tako-server` host only      | Internal management dispatch                      |
| Remote management  | Tailscale IP, `:9844` | Machines in your tailnet     | CLI status, deploy, secrets, upgrade control      |
| SSH                | `tako@host`           | Operators with recovery keys | Setup, enrollment, repair, fallback operations    |

The host you give `tako servers add` is expected to be the server's Tailscale MagicDNS name or Tailscale IP. MagicDNS names are the pleasant path: [Tailscale documents](https://tailscale.com/docs/features/sharing) the shape as `<hostname>.<tailnet-name>.ts.net`, so a box named `ams` can be added by name instead of by a `100.x.y.z` address.

```bash
tako servers add ams.example-tailnet.ts.net
```

If your workstation resolves the short MagicDNS name, the short host is fine too:

```bash
tako servers add ams
```

Both commands start the same add flow. Tako checks the private management endpoint, verifies `tako@host` recovery access, and writes the server only after those probes pass. If the server is new or needs repair, the wizard asks before installing `tako-server`, creating the restricted `tako` and `tako-app` users, authorizing the SSH public key, and enrolling the same key for signed remote management.

The important behavior is the refusal path. If the host does not resolve into Tailscale address space, or the private management probe fails, `tako servers add` does not quietly save a half-working server. It tells you remote management requires Tailscale and asks you to connect both machines first.

## What actually gets signed

The HTTP endpoint is intentionally small:

```text
POST http://<tailscale-host>:9844/rpc
Content-Type: application/json
```

The body is the same typed protocol Tako already uses over the Unix socket. A status request, a deploy command, a secrets update, and a server upgrade control message are still `tako_core::Command` values. HTTP is the transport, not a second API.

Only two commands are public probes:

| Command         | Auth required? | Why it is public                                          |
| --------------- | -------------- | --------------------------------------------------------- |
| `hello`         | No             | Protocol/capability check before the CLI knows much       |
| `server_info`   | No             | Runtime identity and service metadata for add/probe flows |
| Everything else | Yes            | Reads or changes app/server state                         |

For every non-probe command, the CLI signs the exact JSON body it is about to send. It loads usable private keys from `~/.ssh/id_ed25519`, `id_rsa`, or `id_ecdsa`, and also asks `ssh-agent` for available public keys. If a private key needs a passphrase, interactive runs can prompt, and one-line commands can pass `--ssh-passphrase`.

The signed request carries four headers:

| Header                   | Purpose                                                                 |
| ------------------------ | ----------------------------------------------------------------------- |
| `x-tako-key-fingerprint` | Selects the enrolled SSH public key on the server                       |
| `x-tako-timestamp`       | Keeps signatures inside a short freshness window                        |
| `x-tako-nonce`           | Prevents replaying the same signed request                              |
| `x-tako-signature`       | SSH signature over Tako's management-auth context plus the request body |

On the server, `tako-server` reads `/opt/tako/management-authorized-keys`, finds the public key with the matching SHA-256 fingerprint, reconstructs the signed message, verifies the SSH signature, checks that the timestamp is fresh, and rejects reused nonces. The signing namespace is separate from normal SSH login, so a signature is not just "some bytes signed by a key"; it is a signature for Tako management RPC v0.

Here is the flow without the hand-waving:

```d2
direction: right

cli: "tako CLI" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

key: "SSH key\nor ssh-agent" {
  style.fill: "#9BC4B6"
}

tailnet: "Tailscale\nprivate network" {
  style.fill: "#E88783"
}

http: "tako-server\nHTTP :9844" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

auth: "verify fingerprint\ntimestamp nonce signature" {
  style.fill: "#9BC4B6"
}

dispatch: "typed Command -> Response\ndispatcher" {
  style.fill: "#FFF9F4"
  style.stroke: "#2F2A44"
}

cli -> key: "sign JSON body"
cli -> tailnet: "POST /rpc"
tailnet -> http: "private HTTP"
http -> auth: "non-probe command"
auth -> dispatch: "accepted"
```

This is not OAuth, not a dashboard session, and not a second identity system. It is a small extension of the operator key model self-hosted developers already use. If a key can install and recover a Tako server, that same key can be enrolled to sign management commands. If the key is not enrolled, the server says no.

## Why this is better than SSH for normal operations

SSH is still there. The Tako server installer uses it. `tako servers add` verifies `tako@host` recovery access. Upgrade and reload paths still have host-level pieces that need the restricted maintenance helpers installed by the server installer.

But once the server is enrolled, common operations should feel like talking to an API because they are API-shaped operations:

| Operation             | Old shape                             | New shape                                                |
| --------------------- | ------------------------------------- | -------------------------------------------------------- |
| `tako servers status` | Connect through SSH-shaped management | Signed HTTP query over Tailscale                         |
| Server add probe      | SSH check plus socket probing         | Tailscale host check, public probe, signed command probe |
| App state reads       | Shell transport around typed data     | Direct typed response from `tako-server`                 |
| Mutating commands     | Operator login path                   | Body-signed RPC with nonce and timestamp                 |

For users, the difference should mostly be that status and server discovery get quieter. `tako servers status` does not need a project directory; it reads your global server inventory and queries each configured host through signed remote management. The output is still the thing you care about: server health, app state, routes, instance counts, builds, and deploy timestamps.

For Tako, this opens a nicer future. Deploys, logs, server upgrades, secrets sync, and eventually richer platform primitives can share one management transport instead of growing more SSH glue. The docs already describe Tako as the platform layer between your code and the internet: [deployment](/docs/deployment), routing, TLS, secrets, local dev, channels, and workflows. A platform layer needs a control plane that can grow without turning every feature into a remote shell script.

There is a security reason too. The public internet should not be the default place to put server control APIs. A self-hosted tool can ask for a tiny bit more intentional setup: install Tailscale, add the server by MagicDNS name, keep SSH as recovery, and let normal management traffic stay inside the private network.

## The boring control plane

The design is deliberately unflashy.

Remote management is bound to a Tailscale address. The port is fixed. Public probes are minimal. Mutating commands are signed. Nonces are remembered. SSH remains the recovery path. The same typed command dispatcher handles both local and remote management, so there is less behavior drift between "on the box" and "from your laptop."

That is the kind of boring we want for self-hosted infrastructure. Your VPS should feel like something you own, not a bag of shell sessions. Your deploy tool should know how to reach it privately, prove which operator is asking, and then get out of the way.

Tako now has that foundation.
