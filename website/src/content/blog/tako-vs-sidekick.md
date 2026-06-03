---
title: "Tako vs Sidekick"
seoTitle: "Tako vs Sidekick for VPS App Deploys"
date: "2026-04-12T05:21"
description: "Sidekick turns a VPS into a Docker-powered mini-PaaS. Tako skips Docker entirely. Here's how the two CLI deploy tools compare."
image: bd807fb514a9
---

[Sidekick](https://github.com/MightyMoud/sidekick) markets itself as "your own Fly.io" — a Go CLI that turns a VPS into a mini-PaaS with Docker, Traefik, and automatic SSL. At 7.3k GitHub stars, it's one of the more popular tools in the self-hosted deploy space, and for good reason: `sidekick init` sets up a fresh Ubuntu box in about two minutes. That's a great pitch.

Tako does the same job — get your app running on your own server — but makes fundamentally different choices about how to get there. No Docker, no Traefik, no container registry. Let's look at what that means in practice.

## At a glance

|                        | **Sidekick**                       | **Tako**                                                |
| ---------------------- | ---------------------------------- | ------------------------------------------------------- |
| **Deploy method**      | Docker build → SSH transfer        | Build locally → SFTP upload                             |
| **Server requirement** | Ubuntu + Docker + Traefik          | Any Linux box with SSH                                  |
| **Proxy**              | Traefik (Go)                       | Pingora (Rust, Cloudflare)                              |
| **CLI language**       | Go                                 | Rust                                                    |
| **Config format**      | Dockerfile + CLI prompts           | TOML ([`tako.toml`](/docs/tako-toml/))                  |
| **Local dev**          | None                               | Built-in HTTPS + DNS ([`tako dev`](/docs/development/)) |
| **SDK**                | None                               | [JS/TS and Go SDKs](/docs/)                             |
| **Scale-to-zero**      | No                                 | Yes, with cold start                                    |
| **Multi-server**       | Recent addition (select at deploy) | Declarative per-environment                             |
| **Secrets**            | sops + age encryption              | AES-256-GCM, delivered via fd 3                         |
| **Preview envs**       | Yes (git-hash subdomains)          | Yes (per-environment routing)                           |
| **Stars**              | ~7.3k                              | New kid on the block                                    |

## Where Sidekick shines

Sidekick's onboarding is genuinely impressive. Run `sidekick init`, point it at a VPS, and it installs Docker, configures Traefik, sets up SSL, and hardens SSH — all in one command. For someone who's never deployed to a VPS before, that's a powerful "it just works" moment.

The Docker model has real advantages too. If your app already has a Dockerfile, Sidekick doesn't care what language or runtime you're using. Node, Go, Python, Rust — if it builds in Docker, Sidekick can deploy it. That's broad compatibility for free.

Preview environments are a nice touch: `sidekick deploy preview` tags a Docker image with the current git commit hash and spins it up on a subdomain. Quick way to share a branch with your team.

And the secret management approach — encrypting `.env` files with sops and age, tracking checksums so only changed secrets get re-encrypted — is practical and well thought out.

## Where Tako is different

### No Docker required

Sidekick needs Docker on both your local machine (for building images) and the server (for running them). The Dockerfile is the deployment contract — if you don't have one, you can't deploy. On the server side, Sidekick requires Ubuntu specifically; Debian support has been requested but isn't available.

Tako doesn't use Docker at all. You build locally with your runtime's native toolchain, and the artifact goes straight to the server over SFTP. The server just needs SSH access — any Linux distribution, any architecture. No Docker daemon running in the background, no container overhead, no Dockerfile to maintain.

```d2
direction: right

sidekick: Sidekick {
  direction: down

  build: Docker build {style.fill: "#E88783"; style.font-size: 18}
  transfer: SSH transfer {style.fill: "#E88783"; style.font-size: 18}
  docker: Docker run {style.fill: "#E88783"; style.font-size: 18}

  build -> transfer: image
  transfer -> docker: start
}

tako: Tako {
  direction: down

  build: Native build {style.fill: "#9BC4B6"; style.font-size: 18}
  sftp: SFTP upload {style.fill: "#9BC4B6"; style.font-size: 18}
  process: Native process {style.fill: "#9BC4B6"; style.font-size: 18}

  build -> sftp: artifact
  sftp -> process: start
}
```

### A proxy built for production

Sidekick uses Traefik, which is a solid reverse proxy — automatic SSL, Docker-aware routing, wide community adoption. But Traefik is a general-purpose proxy designed for container orchestration. It's powerful, but it's also heavy for the single-server or few-server use case.

Tako uses [Pingora](/blog/pingora-vs-caddy-vs-traefik/), Cloudflare's Rust proxy framework — the same technology that handles a significant chunk of internet traffic. TLS termination, HTTP/2, WebSocket proxying, and health-check-based routing all happen in the same process. No sidecar containers, no separate proxy configuration to manage.

### Scale-to-zero

Sidekick keeps your containers running. If you've got a staging environment, an internal dashboard, and a webhook handler all on one VPS, they're all consuming memory whether anyone's using them or not.

Tako supports [on-demand scaling](/docs/how-tako-works/): instances spin down after an idle timeout and cold-start on the next request. For apps that don't need to be always-on, this is meaningful resource savings — especially on a [$5 VPS](/blog/your-5-dollar-vps-is-more-powerful-than-you-think/) running multiple apps.

### Local development included

Sidekick is a deployment tool — there's no `sidekick dev`. Local development means running Docker Compose yourself or using whatever your framework provides.

[`tako dev`](/docs/development/) gives you real HTTPS with trusted certificates, local DNS routing (`*.test`), and a proxy that matches production behavior. Your app runs the same way locally as it does on the server — same SDK, same process model, same routing. One command, no setup.

### Declarative multi-server

Sidekick recently added multi-VPS support, letting you select which server to deploy to at deploy time. It's a step forward, but server assignment is still a runtime choice rather than a configured state.

Tako makes server membership [declarative in `tako.toml`](/docs/deployment/):

```toml
[envs.production]
route = "api.example.com"
servers = ["la", "nyc"]

[envs.staging]
route = "staging.example.com"
servers = ["staging"]
```

`tako deploy` sends the right build to the right servers automatically. No prompts, no remembering which server runs what.

## Different trajectories

Sidekick is a clever tool that solves a real problem — and we appreciate that it's helped more developers discover self-hosted deployment. The "init a VPS in two minutes" experience is genuinely great.

That said, Sidekick's development has slowed significantly — the last tagged release was October 2024, and commits are sparse. Major requested features like Docker Compose support and database management remain unimplemented. For side projects and single-container apps, it works well. For growing production workloads, the runway is uncertain.

Tako is headed somewhere different. Today it handles deployment, routing, TLS, secrets, and local dev. The roadmap includes backend primitives — WebSocket channels, queues, workflows — things most apps bolt on as separate services. Combined with [multi-server environments](/docs/deployment/) and Cloudflare smart routing, Tako lets you build your own edge network on commodity hardware.

The question is what you need: a quick way to ship a Dockerized app to a VPS, or a platform that grows with your app. Both are valid answers.

Check out [how Tako works](/docs/how-tako-works/) to see the full architecture, or the [CLI docs](/docs/cli/) to get started.
