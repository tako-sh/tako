---
title: "Build a Distributed Web App with Tako and CockroachDB"
date: "2026-04-10T06:18"
description: "Three VPS boxes, Tako for the app layer, CockroachDB for the data layer — a fully distributed stack running on hardware you own, with no managed plan underneath."
image: f81c378820bd
---

We've been telling the story of [building your own edge network on commodity hardware](/blog/build-your-own-edge-network-on-commodity-hardware) — three VPS boxes, [one `tako.toml`](/blog/one-config-many-servers), Cloudflare routing users to the nearest one. It's a nice story until your users start actually writing data. Because as soon as your database lives in one region, every query from Tokyo still round-trips to us-east-1 and the edge only got you halfway.

The other half is a database that can live on all three boxes at once. CockroachDB is one of the nicest answers to that we've found, and it pairs neatly with Tako.

## Why CockroachDB

It speaks the PostgreSQL wire protocol, so your ORM doesn't know it's not Postgres. Underneath, it's a distributed, strongly consistent SQL database — nodes join a cluster, ranges are replicated via Raft, and each node can serve queries locally. You run one `cockroach` binary on three boxes and you have a single logical database that survives a whole region disappearing.

It's also a single self-hosted binary ([github.com/cockroachdb/cockroach](https://github.com/cockroachdb/cockroach)) that installs alongside anything else you already run. No managed tier required to get started.

## The topology

```d2
direction: right

users: Users {shape: rectangle; style.font-size: 14}
cf: Cloudflare {shape: cloud; style.fill: "#9BC4B6"; style.font-size: 14}

la: LA VPS {
  style.fill: "#FFF9F4"
  app: tako-server + app {shape: hexagon; style.fill: "#E88783"; style.font-size: 13}
  db: cockroach {shape: cylinder; style.fill: "#FFF9F4"; style.font-size: 13}
  app -> db: localhost:26257
}

fra: Frankfurt VPS {
  style.fill: "#FFF9F4"
  app: tako-server + app {shape: hexagon; style.fill: "#E88783"; style.font-size: 13}
  db: cockroach {shape: cylinder; style.fill: "#FFF9F4"; style.font-size: 13}
  app -> db: localhost:26257
}

sgp: Singapore VPS {
  style.fill: "#FFF9F4"
  app: tako-server + app {shape: hexagon; style.fill: "#E88783"; style.font-size: 13}
  db: cockroach {shape: cylinder; style.fill: "#FFF9F4"; style.font-size: 13}
  app -> db: localhost:26257
}

users -> cf
cf -> la.app: nearest
cf -> fra.app: nearest
cf -> sgp.app: nearest

la.db <-> fra.db: raft
fra.db <-> sgp.db: raft
sgp.db <-> la.db: raft
```

Each VPS runs two things: `tako-server` — which runs your app, terminates TLS, and handles routing — and a `cockroach` node joined to the cluster. Your app connects to CockroachDB at `localhost:26257`, so every query goes to the local replica. CockroachDB handles cross-region replication and consensus in the background.

Two tools, one responsibility each, no control plane stacked on top. Tako runs the apps. CockroachDB runs as its own systemd service — started on each box with something like:

```bash
cockroach start \
  --certs-dir=/etc/cockroach/certs \
  --advertise-addr=<public-ip> \
  --join=la.example.com,fra.example.com,sgp.example.com \
  --locality=region=eu-central,zone=fra
```

The `--locality` flag is what makes this multi-region instead of just "three boxes running the same database" — CockroachDB uses it for replica placement, follower reads, and its multi-region topology patterns.

## The Tako side

Everything Tako needs lives in one [`tako.toml`](/docs/tako-toml):

```toml
name = "myapp"

[build]
run = "bun run build"

[envs.production]
route = "myapp.com"
servers = ["la", "fra", "sgp"]
```

And a single secret every server shares:

```bash
tako secrets set DATABASE_URL --env production
# postgres://myapp@localhost:26257/myapp?sslmode=verify-full
tako deploy
```

Tako [builds the artifact once](/docs/deployment), SFTPs it to all three boxes in parallel, and each server runs its own rolling update. `DATABASE_URL` is [encrypted at rest and injected via fd 3](/blog/secrets-without-env-files) — no `.env` file, no plaintext on disk — and it's identical across regions because every node is reachable at `localhost`. Your app code never has to know which region it's running in.

## Making reads actually fast

Having a local replica isn't magic on its own — writes still need quorum, and not every read is served locally by default. CockroachDB ships a few [multi-region topology patterns](https://www.cockroachlabs.com/docs) to tune that trade-off:

| Pattern                      | Good for                                                            |
| ---------------------------- | ------------------------------------------------------------------- |
| **Follower reads**           | Low-latency reads that tolerate a small staleness window            |
| **Geo-partitioned replicas** | Tables with a clear regional home (e.g. user rows pinned by region) |
| **Follow-the-workload**      | Apps where the "active" region shifts throughout the day            |

For a typical web app, follower reads are the cheapest win. One clause and most of your read path stops crossing oceans:

```sql
SELECT * FROM users
  AS OF SYSTEM TIME follower_read_timestamp()
  WHERE id = $1;
```

Writes still cross regions for consensus, but the reads — usually the bulk of traffic — come from the same box your user is already talking to.

## Who does what

| Layer             | Tool                                   | Runs where                     |
| ----------------- | -------------------------------------- | ------------------------------ |
| DNS + geo-routing | Cloudflare                             | Cloudflare edge                |
| TLS, proxy, app   | [Tako](/docs/how-tako-works)           | Each VPS                       |
| Application       | Your code + the [`tako.sh` SDK](/docs) | Each VPS, managed by Tako      |
| Data              | CockroachDB                            | Each VPS, as a systemd service |

Nothing you don't own. Nothing you can't `ssh` into.

## The point

Tako is an app platform, not a database — we're happy with that split. Our job is to make the stuff around your code (routing, TLS, secrets, rolling updates, [local dev with real HTTPS](/blog/local-dev-with-real-https)) disappear, so you can pair it with whatever you want underneath. CockroachDB is a great fit because its "drop a binary on every box" model matches Tako's.

Three regions, two binaries per box, one config file, one passphrase-derived key for the secrets. The whole stack sitting on hardware you pay by the month for. No Kubernetes, no managed database bill, no control plane to babysit — just processes and data, where you want them.

[Get started →](/docs)
