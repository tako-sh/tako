---
title: "Why We Don't Default to Docker (and When You Should Still Use It)"
date: "2026-03-22T12:00"
description: "Why we skip the Docker rebuild loop — and when containers still make sense."
image: 9970fc7ceab1
---

You change one line of code. You run the deploy. Then you wait.

The Docker image rebuilds. Layer cache misses because you touched `package.json`. The image pushes to a registry. The server pulls it back down. The container restarts. Three minutes later, your one-line fix is live.

We've all been there. That feedback loop is what led us to build Tako differently.

## Docker Is Great

Let's be clear: Docker is an incredible piece of engineering. It solved real problems — reproducible builds, portable environments, dependency isolation. The container ecosystem is massive and battle-tested. If you're running microservices at scale, orchestrating heterogeneous workloads, or need strict process isolation, containers are a proven choice.

We're not anti-Docker. We just think it shouldn't be the default for every deployment.

## Where Containers Add Overhead

For many web applications — especially JavaScript/TypeScript apps — the container abstraction introduces friction that doesn't pay for itself:

**Build time.** A typical Dockerfile rebuilds layers sequentially. Change a dependency? Invalidate the cache from that layer down. Multi-stage builds help, but they add complexity and still take time. In practice, Docker builds can add minutes to every deploy cycle.

**Registry round-trips.** Push the image. Pull the image. For a 500MB Node.js image, that's real bandwidth and real time, especially if your CI and server aren't colocated.

**Cold start latency.** Container startup isn't free. The runtime needs to pull layers (if not cached), set up the filesystem overlay, initialize networking, and then start your actual process. For scale-to-zero workloads, this overhead compounds.

**Another abstraction to debug.** Dockerfiles look simple until they aren't. Layer ordering, cache busting, multi-stage builds, Alpine vs Debian, permission issues — it's a whole skill tree. When something breaks, you're debugging the build system instead of your app.

Here's a concrete example of what a small change looks like in each model:

| Step        | Docker-based deploy                        | Tako deploy                       |
| ----------- | ------------------------------------------ | --------------------------------- |
| Code change | Edit one file                              | Edit one file                     |
| Build       | Rebuild image layers (~60-120s)            | Run build command locally (~1-5s) |
| Transfer    | Push to registry, pull on server (~30-60s) | SFTP compressed archive (~2-5s)   |
| Start       | Pull layers, start container (~10-30s)     | Start process (~0.1-0.5s)         |
| **Total**   | **~2-4 minutes**                           | **~5-10 seconds**                 |

_Times are illustrative and vary by setup, image size, and caching. Your mileage will differ — but the gap is real._

## Tako's Approach: Direct Execution

Tako runs your app as a process, not a container. When you run [`tako deploy`](/docs/deployment), here's what happens:

1. Your project is copied to a clean working directory (respecting `.gitignore`)
2. Build commands run locally on your machine
3. The result is compressed and sent to your server via SFTP
4. The server installs production dependencies and starts your app directly

No image builds. No registry. No container runtime. Your app runs under the same [runtime](/docs/how-tako-works) (Bun or Node) in both development and production.

This means:

- **Fast iteration** — change, build, deploy in seconds
- **Simple debugging** — SSH in, look at the process, tail the logs
- **Lower resource usage** — no Docker daemon, no overlay filesystem, no image layers
- **True dev/prod parity** — [`tako dev`](/docs/development) uses the same runtime and SDK as production

The tradeoff is intentional. We support a focused set of runtimes — currently [Bun and Node.js](/docs/how-tako-works) for JavaScript apps, plus Go — and optimize deeply for them, rather than supporting anything-in-a-container at the cost of speed.

## When Docker Is the Right Call

There are legitimate reasons to reach for containers:

- **Complex native dependencies** — if your app needs system libraries, GPU drivers, or specific OS packages that are painful to manage directly
- **Strict isolation requirements** — multi-tenant environments where process-level isolation isn't enough
- **Existing infrastructure** — your team already has a container pipeline, registries, and tooling that works well
- **Heterogeneous stacks** — you're running Go, Rust, Python, and Java services and need a single deployment model

We respect these use cases. Docker as an opt-in runtime option is on our [roadmap](https://github.com/lilienblum/tako/issues/8) — because the right tool depends on the job, and sometimes that tool is a container.

The key word is _opt-in_. We think the default should be the fastest, simplest path. For most web apps, that's direct execution.

## Choose Your Abstraction

This isn't Docker vs Tako. It's about choosing the right level of abstraction for your workload.

If you need containers, use containers. If you want fast deploys, minimal overhead, and a tool that gets out of your way — [give Tako a try](/docs/quickstart). One `tako.toml`, one command, and your app is live.

```bash
tako init
tako deploy
```

That's it. No Dockerfile required.
