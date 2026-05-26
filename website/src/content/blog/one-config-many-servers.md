---
title: "One Config, Many Servers"
date: "2026-04-05T14:04"
description: "One tako.toml, two environments, three servers across regions — how Tako takes a side project all the way to a real production setup."
image: 5dc15ac9f3c7
---

Your side project works. It's deployed on one VPS, it has real HTTPS, you're happy. Now someone actually depends on it, and the questions start piling up: where do I test changes before prod? Where does the second server go when the first one falls over? How do I point staging at a different database without duplicating the whole config?

Tako's answer is: add a few lines to your `tako.toml`. One config file describes every environment you run and every server each environment lives on. Deploys know the difference, rolling updates happen in parallel, and rollback is a single command.

## One config, many environments

Environments are declared under `[envs.<name>]`. Each gets its own routes, its own server list, and its own idle-scaling policy. Here's a real-shaped [`tako.toml`](/docs/tako-toml/):

```toml
name = "myapp"

[build]
run = "bun run build"

[vars]
LOG_FORMAT = "json"

[vars.production]
API_URL = "https://api.myapp.com"

[vars.staging]
API_URL = "https://api.staging.myapp.com"

[envs.production]
route = "myapp.com"
servers = ["la", "nyc", "fra"]
idle_timeout = 600

[envs.staging]
route = "staging.myapp.com"
servers = ["staging"]
idle_timeout = 60
```

`[vars]` is the base, `[vars.<env>]` layers on top. Staging gets aggressive idle timeouts so it scales to zero almost immediately — [no resources wasted on code nobody is looking at](/blog/scale-to-zero-without-containers/). Production gets longer warm windows and a three-server fleet.

The same server name can host multiple environments of the same app. `staging.myapp.com` and `myapp.com` could both live on one box if you want — Tako keeps them separated on disk under `/opt/tako/apps/myapp/staging` and `/opt/tako/apps/myapp/production`, with independent processes, secrets, and release histories.

## Deploying to an environment

`tako deploy` defaults to `production`. To ship staging instead:

```bash
tako deploy --env staging
```

Tako builds the artifact once, then uploads and starts it on every server listed under `[envs.staging].servers` **in parallel**. Each server runs its own [rolling update](/docs/deployment/): start a new instance, wait for the SDK readiness signal, drain the old one, repeat. If `fra` falls behind because of network weather, `la` and `nyc` don't wait for it — partial failures are reported at the end and successful servers stay on the new release.

```d2
direction: right

laptop: tako deploy --env production {
  shape: rectangle
}

artifact: Build artifact {
  shape: document
}

la: la {shape: hexagon}
nyc: nyc {shape: hexagon}
fra: fra {shape: hexagon}

laptop -> artifact: build once
artifact -> la: SFTP + rolling update
artifact -> nyc: SFTP + rolling update
artifact -> fra: SFTP + rolling update
```

Secrets follow the same model. They're [encrypted locally](/docs/cli/), keyed per environment, and pushed to each server over the management socket. Tako hashes the local secrets and asks each server whether they match before sending anything — if they do, the deploy skips the secrets payload entirely. New servers and drifted ones are caught automatically.

## Scaling per environment, per server

Instance counts are runtime state, not config. You set them with [`tako scale`](/docs/cli/):

```bash
# two warm instances on every production server
tako scale 2 --env production

# but LA is the big one — bump it to six
tako scale 6 --server la --env production

# staging stays on-demand (scale to zero)
tako scale 0 --env staging
```

These counts persist across deploys, rollbacks, and server restarts, stored on each server rather than baked into `tako.toml`. That means a production hotfix can't accidentally undo last night's scale-up decision.

## Adding a region later

The nice thing about declaring servers per environment is that growing is just a list edit. Register the new server globally once:

```bash
tako servers add <host>
```

Add its name to `[envs.production].servers`, run `tako deploy`, and your app is now serving from the new region alongside the existing ones. Point the DNS record for `myapp.com` at all three IPs (or front them with Cloudflare and let smart routing pick the nearest), and you've got your own edge network running on commodity VPS boxes. No Kubernetes, no orchestrator, no control plane to babysit.

Tako is aiming to be more than a deploy tool — the same `tako.toml` that describes your fleet today will describe [channels, queues, and workflows](/blog/why-tako-ships-an-sdk/) tomorrow. Environments and multi-server deploys are the floor, not the ceiling.

Read the [deployment docs](/docs/deployment/) for the full story, or [how Tako works](/docs/how-tako-works/) for the architecture underneath.
