---
title: "What Happens When You Run tako deploy"
date: "2026-04-12T05:40"
description: "The full sequence from command to live traffic — build, upload, swap, drain — in about 10 seconds."
image: 40a61319c723
---

The whole thing takes about 10 seconds. You type the command, your terminal fills with a progress tree, and then it's done — new version live, zero requests dropped.

But a lot happens in those 10 seconds. Here's the full sequence, from keystroke to live traffic.

## Build and preflight run in parallel

The moment you hit enter, two things happen simultaneously.

**On your machine**, Tako copies your project to a clean build directory (respecting `.gitignore`), symlinks `node_modules/` from your original tree for speed, and runs your build command. The output gets compressed into a [Zstandard](https://facebook.github.io/zstd/) archive — roughly 3–5x smaller than gzip, and faster to decompress.

**Over SSH**, Tako connects to your server(s), checks the architecture (x86_64 or aarch64), confirms `tako-server` is healthy, and pre-establishes the connections that'll be reused for the upload.

Build and preflight run as concurrent tasks. By the time your build finishes, the server connection is already warm.

## Upload and prepare

The compressed artifact ships to your server via SFTP, landing at `/opt/tako/apps/{app}/releases/{version}/`. If you're re-deploying the same version (common while debugging), Tako detects the existing directory and skips the upload entirely.

Once extracted, the server runs a **PrepareRelease** phase: download the runtime binary ([Bun or Node](/docs/how-tako-works/)) if it isn't cached, then install production dependencies. This all happens _before_ any instance swap — dependency installation doesn't eat into your zero-downtime window.

[Secrets](/blog/secrets-without-env-files/) get a shortcut too. Tako hashes your encrypted secrets and compares against what the server already has. Same hash? Skip the transmission. Changed? They're included in the deploy command and delivered to each new instance via file descriptor 3 — never written to disk on the server.

## The swap

This is where the [zero-downtime rolling update](/blog/zero-downtime-deploys-without-a-container-in-sight/) happens. Tako sends a `Deploy` command over the server's unix socket, and the server replaces instances one at a time:

```d2
direction: right

build: Build {style.fill: "#9BC4B6"; style.font-size: 16}
upload: Upload {style.fill: "#9BC4B6"; style.font-size: 16}
prepare: Prepare {style.fill: "#9BC4B6"; style.font-size: 16}
spawn: Spawn {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
ready: Ready {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
health: Healthy {style.fill: "#FFF9F4"; style.stroke: "#2F2A44"; style.font-size: 16}
drain: Drain {style.fill: "#E88783"; style.font-size: 16}
live: Live ✓ {style.fill: "#9BC4B6"; style.font-size: 16}

build -> upload: SFTP
upload -> prepare: extract + install
prepare -> spawn: Deploy command
spawn -> ready: "TAKO:READY:port"
ready -> health: probe /status
health -> drain: old instance
drain -> live: kill old
```

For each running instance, one at a time:

1. **Spawn** — a new process starts with `PORT=0` (OS-assigned) and a unique internal token
2. **Ready** — the [SDK](/docs/) writes `TAKO:READY:12345` to stdout once your app is genuinely ready to serve, not just listening on a port
3. **Health** — the server probes the SDK's built-in `/status` endpoint
4. **Drain** — the old instance stops receiving new requests; in-flight requests finish (up to 30s)
5. **Kill** — old process exits

If the new instance fails to start or its health check times out, Tako kills it and keeps the old instances running. Automatic rollback, no intervention needed.

## The numbers

| Phase         | Typical time                        |
| ------------- | ----------------------------------- |
| Build         | 1–5s (runs locally)                 |
| Upload        | 2–5s (Zstandard over SFTP)          |
| Prepare       | 0s (cached) to ~10s (fresh install) |
| Instance swap | 100–500ms per instance              |
| **Total**     | **~5–15s**                          |

After the swap, a few housekeeping tasks run in the background: the `current` symlink atomically points to the new release, and release directories older than 30 days get pruned.

The whole design optimizes for one thing: get your code change live as fast as possible, without dropping a single request. No Docker, no image registry, no container runtime — just your code, an SFTP transfer, and a [Pingora-powered](/blog/pingora-vs-caddy-vs-traefik/) proxy that knows how to swap processes gracefully.

Read the [deployment guide](/docs/deployment/) for setup, [how Tako works](/docs/how-tako-works/) for the full architecture, or the [CLI reference](/docs/cli/) for all the flags `tako deploy` accepts.
