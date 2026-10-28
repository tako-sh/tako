---
title: Documentation
description: "Find the Tako task, primitive, or reference you need: agent setup, local development, deployments, secrets, storage, live data, and operations."
---

Tako runs applications on local HTTPS and deploys them to your own servers. Use this index to select the relevant topic instead of loading the entire documentation set.

## Read with an agent

Read the project's `AGENTS.md` and selected `tako.toml` first. With Tako MCP connected, use `tako_docs` to list topics or read `docs/<topic>`, and `tako_search_docs` for literal text search. Bundled docs match the installed CLI; use its tool schemas or `tako <command> --help` for exact arguments.

On the web, use [llms.txt](/llms.txt) to discover topics and the `.md` version of a page for source text, for example [secrets.md](/secrets.md). The [JSON API](/openapi.json) lists and retrieves pages. Use the matching [SDK skill](https://tako.sh/skills.md) for JavaScript, Go, or Rust APIs.

## Start here

- [Quickstart](/quickstart/): install, initialize, run locally, and deploy.
- [Coding agents](/agents/): install skills, connect MCP, and use version-matched docs.
- [Framework guides](/framework-guides/): Next.js, TanStack Start, Vite, fetch handlers, Go HTTP frameworks, and Rust/Axum.
- [Docker](/docker/): package an SDK-aware image and deploy it with Tako.

## Develop an app

- [Local development](/development/): start and inspect apps, use HTTPS routes, and run one-off scripts.
- [Secrets](/secrets/): set app secrets, access them through an SDK, and rotate deployed values.
- [Storage](/storage/): bind local or S3-compatible files and issue upload/download URLs.
- [Images](/images/): optimize public images and generate responsive image URLs.
- [Cache](/cache/): cache server-side JSON results with a TTL.

## Add live data and background work

| Need                                  | Primitive                | Important boundary                                                                      |
| ------------------------------------- | ------------------------ | --------------------------------------------------------------------------------------- |
| Deliver events to every subscriber    | [Channels](/channels/)   | Params do not partition a channel's stream; replay is temporary.                        |
| Track online connections and metadata | [Presence](/presence/)   | Membership is scoped by params; metadata does not establish identity.                   |
| Keep database reads current in the UI | [Stores](/stores/)       | Invalidate after committed writes; Tako does not detect database changes automatically. |
| Execute background jobs or schedules  | [Workflows](/workflows/) | Checkpointed side effects still need idempotency.                                       |

Channels, Presence, and Stores require the `postgres_url` provider credential across multiple servers. Workflows support shared Postgres or intentional per-server execution; each primitive documents its requirements. Application databases, file storage, and caches are separate from this runtime state.

## Deploy and operate

- [Servers](/servers/): provision a host, enroll management access, and maintain the service.
- [Deployment](/deployment/): build artifacts, run migrations, and roll a release into traffic.
- [Routing and TLS](/routing/): configure domains, path routes, certificates, and client-IP policy.
- [Operations](/operations/): inspect status and logs, scale instances, and roll back releases.
- [Backups](/backups/): configure private backup storage, create backups, and restore app data.
- [Troubleshooting](/troubleshooting/): choose diagnostics from the observed failure.

## Look something up

- [CLI](/cli/): every command and flag.
- [tako.toml](/tako-toml/): project configuration.
- [Presets](/presets/): runtime and framework defaults.

## Learn the model

[How Tako works](/how-tako-works/) explains the CLI, server, SDK bootstrap, request routing, and state ownership. Use it when a task depends on a runtime boundary; command, configuration, and primitive topics contain the actionable details.

For product context, see [tako.sh](https://tako.sh). Report bugs or request features in [GitHub issues](https://github.com/tako-sh/tako/issues).
