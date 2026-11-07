---
title: "How to Migrate a Tako VPS from Tailscale to Native WireGuard"
date: "2026-11-06T15:16"
description: "Move an existing Tako VPS to native WireGuard. Preserve app placement, open the right ports, and verify management and recovery SSH before removing Tailscale."
image: 253da8d8143d
imageAlt: "An octopus listens on a switchboard receiver while plugging in a second cable beside the first."
---

If you can SSH into your VPS only by its Tailscale name, keep that session open. It can bootstrap the replacement network. It can also get you back onto the machine if your provider drops the first WireGuard packet. The useful finish line here is a *second, fresh administrator login* over the new network while your existing app still serves its usual URL.

We'll migrate one existing Linux VPS in place. The example uses the legacy inventory name `shop-vps`, the local network alias `shop`, and the public example address `203.0.113.10`. Substitute your own values throughout. If you use NetBird, the same enrollment and verification steps apply. Keep your existing NetBird connection available during the move and use its own cleanup procedure at the end. [How native networks work](/blog/native-wireguard-networks-for-vps-apps/)

## Record what works and open the two paths

Start in the app's project. Save the output of these commands somewhere private. Note the server, app name, environment, running release, instance health, and public routes. Request an actual page or health endpoint whose normal response you recognize. That gives you something specific to check after the server process restarts.

```bash
tako servers ls --json
tako status --json
curl --fail --show-error --silent https://shop.example.com/health
```

Record the `servers` list in each production project. After enrollment, keep those server names alongside the new network ID to preserve placement. Omit the list only if this app should deploy to every trusted member. A Compose environment needs exactly one target, even if more servers belong to its network. [Deployment target selection](/docs/deployment/)

Use a supported Linux host with systemd, glibc, and administrator SSH access. Take your usual recovery snapshot or backup before updating the server, and verify that you can open the provider's recovery console. An already-open SSH connection is handy during enrollment. It cannot fix an unreachable machine after a reboot. Schedule the work for a time when you can check the running apps immediately after the service restarts. [Server setup](/docs/servers/) and [backups](/docs/backups/)

| Allow at the server | Who needs to reach it | Reason |
| --- | --- | --- |
| TCP 443, or your configured HTTPS port | Operator machines and deployment runners, directly | Native management verifies the enrolled server's HTTPS identity |
| UDP 51820, or your chosen WireGuard port | Private network peers | Encrypted WireGuard traffic |
| SSH over the existing recovery path | The administrator doing enrollment | Installation and recovery during the switch |

Apply the rule at **both** the provider firewall and the guest firewall, and ensure the Linux rule survives reboot. TCP 443 and UDP 443 are separate rules even when they have the same number. Choose the UDP port you actually intend to advertise. The private management listener on TCP 9844 belongs inside the private network. Leave it out of the public ingress rules.

On a VPS behind a CDN, your shop can serve HTTPS beautifully while direct connections from your laptop to the origin time out. Test direct TCP access to the advertised server IP. Check for a mandatory `trusted_proxy.proxy_protocol` setting on the HTTPS listener, too. Native management needs a direct TLS connection, so resolve that listener conflict before switching it. Ordinary HTTP forwarding headers from a CDN are a separate setting. [Native listener requirements](/docs/networks/)

For UDP, a port scanner can be inconclusive because WireGuard quietly discards unauthenticated packets. During provider setup, a narrowly filtered packet capture on the guest's public interface can show whether an external probe even arrives. We saw ordinary HTTP arrive on one VPS while UDP 51820 disappeared before it reached Linux. The missing allow rule was in the provider's network security group. For the finished tunnel, use an authenticated handshake and a real request. [WireGuard quick start](https://www.wireguard.com/quickstart/)

## Enroll the existing server and change the project target

Upgrade the workstation CLI, then check that `servers add --help` lists both `--network` and `--wireguard-endpoint`. Use your existing administrator login for the SSH part. A Tailscale MagicDNS name works here while the old connection is active. Give WireGuard the VPS's reachable public address explicitly, especially when the host has a private NIC behind provider NAT.

```bash
tako upgrade
tako network create shop
tako servers add ubuntu@shop-vps --network shop --name shop-vps \
  --wireguard-endpoint 203.0.113.10:51820
tako network status shop --json
```

For a custom public HTTPS listener, supply the existing `--https-port` to enrollment and permit that port at both firewalls. Preserve a custom `--http-port` as well. Enrollment installs the native components, prepares keys on the VPS and records the server's identity and certificate pins. Let it finish on one host before enrolling another. You can leave the old VPN daemon running even after Tako switches its management binding.

**A rollout note for build `883e93d`.** We hit native-agent startup failures on real Linux guests during this migration. The shipped helper can reject an interface it just created or the kernel's numeric route types. The agent can also reject the packaged Corrosion executable or encounter a missing registry table. If enrollment fails, keep the old SSH path and stop before changing project selectors. Check the agent journal from that session and use a release with the relevant fixes or a tested operator repair. Opening more ports cannot repair a local agent that fails to start.

```bash
# On the VPS, through the recovery session
systemctl status tako-network-agent tako-server --no-pager
sudo journalctl -u tako-network-agent -n 30 --no-pager
sudo wg show all listen-port
```

Once `tako network status shop --json` refreshes successfully, check that it names the intended server and laptop. Copy the stable 32-character ID from `tako network list --json`. In each project targeting this VPS, add `network` and set `servers` to the enrolled names that should receive this app. Preserve the existing routes, TLS settings, and other environment fields. This example ID is a placeholder.

```toml
[envs.production]
network = "0123456789abcdef0123456789abcdef"
servers = ["shop-vps"]
route = "shop.example.com"
```

After enrollment, remove the old `[[servers]]` entry for `shop-vps` from `~/.tako/config.toml`. Keep the native network profile and alias. Current CLI versions reject saved `vpn` fields, so the old entry must be removed before listing servers or reading status. Editing this local inventory does not stop the old VPN daemon or close your recovery session. [Removed provider settings](/docs/servers/#choose-the-vpn-provider)

Run `tako status --json` from the project and inspect the **native** server entry, normally named `shop/shop-vps`. Compare the running release with your earlier capture and read a finite log window using `tako logs --env production --days 1 --json`. An unchanged release lets you test the management change without deploying new app code. Request the same public URL again and compare its response. [CLI reference](/docs/cli/)

## Prove recovery access, then retire Tailscale

On your workstation, run the following in an interactive terminal. Tako embeds WireGuard; `connect` explains why it needs administrator access before asking for the local sudo password. Linux needs a TUN device, iproute2, and an active systemd-resolved resolver; macOS uses its native tunnel and scoped resolver. Use the same user account that created the network so Tako can find the enrolled profile. For an enrolled shell alone, `tako ssh` can also run without `connect` or administrator access. [Private connections](/docs/networks/#connect-local-applications)

```bash
tako connect shop
tako ssh shop-vps --network shop -- hostname
```

On the VPS, run `sudo wg show all latest-handshakes` through the original session. After the private SSH request you should see a recent, nonzero timestamp for the workstation peer. Also open a fresh session with your **actual recovery administrator account**, using the VPS's native IPv6 address. You can obtain that address from `ip -6 addr show` on its Tako interface. Verify the host key against your existing trusted session. An enrolled `tako` shell and an administrator who can repair systemd have different privileges. Check the one you will depend on.

If all three checks pass — native management, administrator SSH over WireGuard and the public application — park the old connection before removing the package. Run the next commands **from the new administrator session** on a Debian or Ubuntu VPS installed with APT. Stop after the first command and repeat the checks. If any fail, use the same session or provider console to run `sudo systemctl enable --now tailscaled` and investigate.

```bash
sudo systemctl disable --now tailscaled
# Retest with a fresh connection before proceeding.
sudo apt-get remove tailscale
```

Finally, keep the native profile and run `tako network status shop --json` once more, then verify the public app response. `tako servers remove shop/shop-vps` revokes native network membership and delegated approvals; it is not a cleanup step for this migration. If other people or machines deploy this project, enroll their devices and verify their access before their next release. The migration is complete when the people who operate this app can deploy it, reach it, and recover it through the new setup.
