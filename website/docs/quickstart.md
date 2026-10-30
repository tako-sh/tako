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

Remote deploys require [Tailscale](https://tailscale.com/) or [NetBird](https://netbird.io/), including self-hosted networks. Install your preferred VPN and its CLI on your workstation and deployment host, connect both to the same network, and allow SSH and TCP port `9844` between them in its access policies.

From your developer machine, add the server by its VPN hostname or IP:

```bash
tako servers add admin-user@my-server
```

Setup asks which VPN to use. Connected providers appear first with a `detected` hint, and the first option is preselected. For scripts, pass `--vpn tailscale` or `--vpn netbird`. The choice is saved per server and reused for repair and upgrades. Existing server entries default to Tailscale. See [Servers](/servers/#choose-the-vpn-provider) for NetBird examples, noninteractive selection, and direct installation.

The `admin-user@host` form tells Tako which admin SSH user to use if install or repair is needed. Tako stores the VPN host and uses its first label as the server name. Before saving the server in your global `config.toml`, Tako checks the selected private network, verifies `tako` SSH recovery access, records target metadata, and verifies signed private management.

If `tako-server` is missing or access needs repair, that shorthand bootstraps the host with the provided admin user. Tako starts the service with default listener settings, enrolls the SSH key used for signed management, and installs the libvips runtime used by the image optimizer.

Check remote access:

```bash
tako status
```

To install from the host itself, run the server installer as root and enter your workstation SSH public key when prompted:

```bash
sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
```

The installer bootstraps the host and starts `tako-server`. Run `tako servers add` from your developer machine to verify access and save it locally.

## First deployment

Run the following commands from your app directory.

The app already has a `tako.toml` from local setup. Check its production route and server mapping, and set the app's required [Secrets](/secrets/) and [Storage](/storage/) bindings before deploying.

```bash
tako servers list --json
```

Run `tako agents` anytime to set up or refresh agent support; see [Coding agents](/agents/). If multiple servers exist, map the desired names in `[envs.production].servers`; see [Deployment](/deployment/#configure-an-environment).

`tako dev` uses `{app}.test` by default. Add `[envs.development]` only when you want custom local routes.

Deploy:

```bash
tako deploy --env production
```

Inspect `tako status --json`, request the deployed hostname, and check finite app logs. See [Deployment](/deployment/) for build and rollout behavior and [Operations](/operations/) for status, logs, scaling, and rollback.
