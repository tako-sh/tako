---
title: "Tako vs Temporal"
date: "2026-04-19T11:38"
description: "Temporal defined the durable-workflow model for a generation of backends. Tako borrows the shape but runs it embedded in the per-app server — no cluster, no Cassandra, no Elasticsearch."
image: fd05510ccf49
---

Before Inngest, before Trigger.dev, before any of the JavaScript-native step-function platforms, there was <a href="https://temporal.io" target="_blank" rel="noopener noreferrer">Temporal</a>. The Temporal team (and Cadence before it at Uber) effectively invented the durable-execution category: the idea that you can write ordinary code with sleeps, retries, and signals, and have the runtime persist every step so your business logic survives crashes, deploys, and week-long waits. Every workflow engine shipped since — including [Tako's](/blog/durable-workflows-are-here/) — owes Temporal a debt.

The difference is what comes with the durability.

## At a glance

|                       | **Temporal**                                           | **Tako**                              |
| --------------------- | ------------------------------------------------------ | ------------------------------------- |
| **Deployment model**  | Cluster (Frontend, History, Matching, Worker)          | Embedded in `tako-server`             |
| **Persistence**       | Cassandra, MySQL, or PostgreSQL                        | Per-app SQLite file                   |
| **Visibility**        | Elasticsearch (advanced) or DB (basic)                 | Rows in the same SQLite file          |
| **Workers**           | Separate fleet, any language                           | Subprocess, scale-to-zero             |
| **Durable primitive** | `workflow.sleep`, `workflow.waitForSignal`, activities | `ctx.run`, `ctx.sleep`, `ctx.waitFor` |
| **Billing (cloud)**   | $50 per million Actions                                | None — runs on your VPS               |
| **Open source**       | Yes, MIT (~19.7k stars)                                | Yes, part of Tako                     |
| **Languages**         | Go, Java, TypeScript, Python, PHP, .NET, Ruby          | TypeScript and Go                     |

## What Temporal gets right

Temporal's credentials are not up for debate. Uber, Netflix, Stripe, Snap, Coinbase, Datadog, and a long list of other large engineering orgs run real production workloads on it. The model — determinism-enforced workflow code, activities as side-effect boundaries, event-sourced history — is rigorous in a way that younger engines still look up to. Seven first-class SDKs. A mature Web UI with powerful search. Cross-datacenter replication. Signals, queries, child workflows, schedules, batch operations. When your job is "orchestrate tens of thousands of long-running processes across a platform team of fifty," Temporal is unambiguously the right answer.

The operational cost of that power is the cluster. A production self-hosted Temporal deployment is four Temporal services — Frontend, History, Matching, and Worker — plus a primary database (Cassandra, MySQL, or PostgreSQL) and, for anything beyond basic search, Elasticsearch for the visibility store. The <a href="https://github.com/alexandrevilain/temporal-operator" target="_blank" rel="noopener noreferrer">Kubernetes operator</a> exists for a reason. That's not a knock — it's the price of a real cluster. It's just a price.

## Where Tako is different

### The cluster is a file

Tako's workflow engine runs inside `tako-server` — the same process that already handles your app's HTTP proxy, TLS, secrets, and [scale-to-zero supervision](/blog/scale-to-zero-without-containers/). The "cluster" is one SQLite file at `{tako_data_dir}/apps/<app>/runs.db` with WAL enabled. The "worker fleet" is a subprocess that the supervisor spawns on demand. The "API" is a unix socket.

```d2
direction: right

temporal: Temporal cluster {
  direction: down

  frontend: Frontend
  history: History
  matching: Matching
  worker: Worker service
  db: Cassandra / SQL
  es: Elasticsearch

  frontend -> history
  frontend -> matching
  matching -> worker
  history -> db
  frontend -> es
}

tako: Tako {
  direction: down

  server: tako-server
  runs: runs.db (SQLite)
  sub: Worker subprocess

  server -> runs: persist
  server -> sub: supervise
}
```

One binary, one file, one socket. [`tako deploy`](/blog/what-happens-when-you-run-tako-deploy/) ships HTTP handlers and `workflows/*.ts` in the same release.

### Same vocabulary, smaller surface

Tako's [SDK primitives](/docs/tako-toml/) — `ctx.run` for memoized side effects, `ctx.sleep` for durable waits, `ctx.waitFor` paired with `signal`, and cron via `schedule` — cover retries, long waits, human approvals, fan-out, and scheduled jobs. That's the working set of what most apps need from a workflow engine. We deliberately skipped child workflows, queries, versioning APIs, and history replay knobs. If your system design genuinely needs those, Temporal is the correct tool.

### No per-Action meter

Temporal Cloud bills per Action — roughly every workflow start, signal, activity completion, and heartbeat. At $50 per million Actions, pricing is fair and predictable for the platform it is. But it's a meter that runs on every durable-step equivalent, forever. Tako's workflow engine runs on the [$5 VPS](/blog/your-5-dollar-vps-is-more-powerful-than-you-think/) you were going to pay for anyway. The marginal cost of one more workflow run is the CPU it uses.

## Different ambitions

Temporal is a durable-execution platform, and the cluster, the seven SDKs, and the Cloud product exist because Netflix-scale workloads actually need them. If that's your problem, go use Temporal — it will serve you well for a decade.

Tako is building a different thing: [the platform layer between your code and the internet](/blog/durable-channels-built-in/) for teams who want backend primitives — deploy, TLS, secrets, durable channels, workflows — without standing up a separate cluster for each one. One `tako-server` on one box. One [`tako.toml`](/docs/tako-toml/).

If you've been running Temporal in production for years, we're not here to move you. If you've been staring at the self-host docs thinking "I just need step retries for this side-project," [give Tako a try](/docs/cli/).
