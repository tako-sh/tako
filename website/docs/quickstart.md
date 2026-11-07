---
title: Quickstart
description: "Get started with Tako in minutes: install the CLI, run local HTTPS development, and deploy your first app to your own server."
---

Install Tako, run your app locally, and deploy it to your own server.

## Local setup

Install the CLI:

```bash
curl -fsSL https://tako.sh/install.sh | sh
```

Verify installation:

```bash
tako --version
```

If you install Tako another way and use image optimization locally on macOS:

```bash
brew install vips
```

From an existing app directory, initialize the project:

```bash
tako init
```

`tako init` detects the runtime and preset, writes `tako.toml`, installs the SDK for JavaScript and Go projects, and updates `.gitignore` for the encrypted secrets file. It asks for an app name and production route, offers SSL credential setup for wildcard routes, and offers coding-agent setup. For Rust, use the [Rust SDK skill](https://tako.sh/.well-known/agent-skills/tako-sdk-rust/SKILL.md). Follow [Framework Integration](/framework-guides/) if your framework needs an adapter.

Start local HTTPS development in a terminal:

```bash
tako dev
```

On your first `tako dev` run, Tako asks for your `sudo` password to set up local no-port HTTPS and custom `*.test` URLs. This happens once and is reused for other apps.

Open the URL shown in the terminal header. By default, Tako uses `{app}.test`, where `{app}` is your app name. The longer `{app}.tako.test` form is always available as a fallback.

After first-time local setup, agents should inspect `tako dev list --json` before starting another session, then use `tako dev --background --json` or the `tako_dev` MCP tool for finite startup. Request the returned URL to verify the app. See [Local Development](/development/) for lifecycle details.

## Remote setup

Create a native network and enroll a supported Linux server:

```bash
tako network create production
tako servers add admin@203.0.113.10 --network production --name edge-a --wireguard-endpoint 203.0.113.10:51820
tako network list --json
```

Replace the example IP with your server's reachable IP. Bootstrap uses administrator SSH; subsequent management uses pinned HTTPS with this device's keys. Allow HTTPS and the configured WireGuard UDP port on the server. No external network account or workstation TUN interface is required for deployment. See [Native networks](/networks/) for firewall requirements, device enrollment, and scoped sandbox access.

For an existing external VPN setup, follow the [native re-enrollment steps](/servers/#choose-the-vpn-provider).

## First deployment

Run the following commands from your app directory.

The app already has a `tako.toml` from local setup. Check its production route and network or server selection, and set the app's required [Secrets](/secrets/) and [Storage](/storage/) bindings before deploying.

```bash
tako network list --json
```

Run `tako agents` anytime to set up or refresh agent support; see [Coding agents](/agents/). Set `[envs.production].network` to the stable ID from `network list`. See [Deployment](/deployment/#configure-an-environment).

`tako dev` uses `{app}.test` by default. Add `[envs.development]` only when you want custom local routes.

Deploy:

```bash
tako deploy --env production
```

Inspect `tako status --json`, request the deployed hostname, and check finite app logs. See [Deployment](/deployment/) for build and rollout behavior and [Operations](/operations/) for status, logs, scaling, and rollback.
