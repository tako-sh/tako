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

From your app directory, install the SDK:

```bash
bun add tako.sh
```

Start local HTTPS development:

```bash
tako dev
```

On your first `tako dev` run, Tako asks for your `sudo` password to set up local no-port HTTPS and custom `*.test` URLs. This happens once and is reused for other apps.

Open the URL shown in the terminal header. By default, Tako uses `{app}.test`, where `{app}` is your app name. The longer `{app}.tako.test` form is always available as a fallback.

## Remote setup

Remote deploys require [Tailscale](https://tailscale.com/). Install it on your workstation and deployment host, then connect both to the same tailnet. Use the server's Tailscale MagicDNS name, for example `my-server`.

From your developer machine, add the server by MagicDNS name:

```bash
tako servers add admin-user@my-server
```

The `admin-user@host` form tells Tako which admin SSH user to use if install or repair is needed. Tako stores only the MagicDNS host and uses its first label as the server name. Before saving the server in your global `config.toml`, Tako checks Tailscale, verifies `tako` SSH recovery access, records target metadata, and verifies signed private management.

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

Initialize Tako config:

```bash
tako init
```

`tako init` prompts for app name and production route, writes them to `tako.toml`, offers encrypted SSL setup for wildcard routes, and updates `.gitignore` so `.tako/secrets.json` stays trackable.

`tako dev` uses `{app}.test` by default. Add `[envs.development]` only when you want custom local routes.

Deploy:

```bash
tako deploy
```

See the [tako.toml reference](/tako-toml/) for every config option, or [Framework Guides](/framework-guides/) for adapter examples.
