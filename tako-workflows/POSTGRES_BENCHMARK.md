# Postgres queue churn benchmark

Run `postgres_queue_bench` against a **disposable Postgres 14+ database**. It uses
the actual `RunsDb` enqueue, claim, heartbeat, completion, pruning, and lease
reclamation paths. Each case creates a unique `queue_bench_*` schema and drops
that schema when finished. A killed process may leave its schema behind.

```sh
export TAKO_TEST_POSTGRES_URL=postgres://postgres:local-password@127.0.0.1:5432/queue_bench
cargo run -p tako-workflows --example postgres_queue_bench > queue-bench.jsonl
```

For a local disposable server:

```sh
docker run --detach --name tako-queue-bench-pg \
  --publish 127.0.0.1:55432:5432 \
  -e POSTGRES_PASSWORD=local-password -e POSTGRES_DB=queue_bench \
  postgres:17 -c autovacuum_naptime=1s -c track_io_timing=on
export TAKO_TEST_POSTGRES_URL=postgres://postgres:local-password@127.0.0.1:55432/queue_bench
```

The default matrix compares three lease-index variants, each with normal vacuum
operation and with an open `REPEATABLE READ` transaction that has acquired a
snapshot. Merely opening a transaction without acquiring a snapshot would not
reproduce the pinned-horizon failure mode described by
[PgQue](https://pgque.dev/docs/concepts/).

- `indexed`: the original `(app_id, lease_until)` B-tree over all runs.
- `partial`: the same B-tree restricted to `status='running'`.
- `unindexed`: no lease index; the existing `(app_id, status, run_at)` index
  remains available for finding running jobs.

The benchmark opens all store connections before selecting an index variant, so
schema initialization cannot silently recreate the index during a case.

| Environment variable | Default | Meaning |
| --- | ---: | --- |
| `BENCH_SECONDS` | 30 | Workload duration per case |
| `BENCH_RECOVERY_SECONDS` | 30 | Additional duration with the snapshot released and traffic continuing |
| `BENCH_JOBS_PER_SECOND` | 200 | Target enqueue rate for one producer |
| `BENCH_WORKERS` | 4 | Competing short-job workers, each with its own store connection |
| `BENCH_LONG_JOBS` | 128 | Running jobs renewed by a separate heartbeat connection |
| `BENCH_HEARTBEAT_MS` | 200 | Target interval between heartbeat rounds |
| `BENCH_HISTORY` | 10000 | Retained completed rows seeded before measurement |
| `BENCH_INDEX` | `all` | `indexed`, `partial`, `unindexed`, or `all` |
| `BENCH_HORIZON` | `both` | `normal`, `held`, or `both` |

Heartbeat and cleanup timing is deliberately accelerated: normal SDK heartbeats
are roughly one-third of the lease duration, and production history is retained
for six months. Here short-job history is pruned after two seconds; the seeded
history has future completion timestamps so it remains present. Each case sets
the runs table's vacuum threshold to 50 and scale factor to 0.01. The example
Docker command also uses a one-second autovacuum nap. These settings give vacuum
a chance to keep up in the normal case; they cannot overcome a pinned snapshot.

Each case now has an initial `normal` or `held` phase, then a `recovery_control`
or `recovery` phase. At the transition, the held transaction is rolled back while
all workload threads keep running. There is no manual vacuum after release;
normal autovacuum must catch up. The normal case continues for the same duration
to provide a control for warm caches and time-dependent effects.

JSON `sample` lines contain per-window claim, heartbeat, and reclaim p50/p95/max latency,
enqueue/completion counts, estimated dead tuples, HOT/total updates, autovacuum
count, table/index sizes, and WAL bytes since the current phase started. A
`summary` line aggregates latency and operation counts for each phase. Tuple and
autovacuum counters remain cumulative. Per-window completions can exceed enqueues
because jobs cross window boundaries. Claim timing
covers successful calls, including transaction commit and saved-step lookup;
it is not enqueue-to-dispatch latency. Update counters also include initial
long-job claims. WAL is cluster-wide, so do not run other workloads concurrently.
The target producer and heartbeat rates can be missed under saturation; compare
the actual counts as well as latency and storage.

This is a short diagnostic, not a maximum-throughput or production-capacity
claim. Repeat at representative payload sizes, history size, active-job count,
heartbeat cadence, and duration before making deployment sizing decisions.

## Recorded comparison (2026-10-26)

Postgres 17.11 in local Docker, Rust dev profile, default workload above, one
30-second run per case. Each case completed approximately 6,000 short jobs and
19,200 heartbeats. [Summary data](benchmarks/postgres-queue-summary.json) includes
the actual counts and other counters. Sizes are MiB; latencies are milliseconds.

| Lease index | Snapshot | WAL | All run indexes | Claim p95 | Reclaim p95 | Dead tuples |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Original | Normal | 21.38 | 1.52 | 4.20 | 1.55 | 995 |
| Running only | Normal | 19.82 | 1.41 | 4.11 | 1.67 | 1,098 |
| Removed | Normal | 14.18 | 1.28 | 4.05 | 1.66 | 814 |
| Original | Held | 24.16 | 5.61 | 5.20 | 2.22 | 37,264 |
| Running only | Held | 22.62 | 4.82 | 5.23 | 1.85 | 37,783 |
| Removed | Held | 20.57 | 3.98 | 5.21 | 10.90 | 37,400 |

The selected change is the partial index on running jobs. Compared with the
original, it reduced WAL by 7.3% in the normal case and 6.4% with a held snapshot;
total run-index size fell by 7.7% and 14.1%. Claim latency was similar. Heartbeats
still modify an indexed value and cannot use HOT updates; this change primarily
avoids indexing pending jobs and retained history. SDK renewal timing is unchanged.

Removing the lease index enabled HOT updates under normal vacuum operation and
saved more WAL, but held-snapshot reclaim p95 rose from 2.22 ms to 10.90 ms. Under
that snapshot, almost all heartbeats still needed non-HOT updates as old tuple
versions occupied pages. Removing the index is therefore not the default.

All three variants accumulated roughly 37,000 dead tuples with a held snapshot,
despite frequent autovacuum. The partial index does **not** eliminate the bloat
failure mode. Monitor long-lived snapshots, queue latency, dead tuples, and
consumer progress. This workload kept up with its target enqueue rate; it did
not demonstrate queue collapse or establish the maximum sustainable rate.

On upgrade, store initialization builds `idx_runs_app_running_lease` and drops
the legacy `idx_runs_app_lease` in its existing serialized schema transaction.
That first index build scans the table and can temporarily block writes on a
large store. Subsequent initializations reuse the new index. Existing lease
expiry queries already specify `status='running'`, so they can use the partial
index without query or protocol changes.

## Recovery comparison (2026-10-26)

The selected partial index was exercised with a 30-second initial phase followed
by 30 seconds of continued traffic. The same Postgres 17.11 setup and default
workload were used. [Recovery data](benchmarks/postgres-queue-recovery.json)
contains both cases' summaries and the per-second recovery samples.

| Phase | Dead tuples at summary | Table MiB | Index MiB | Claim p95 ms |
| --- | ---: | ---: | ---: | ---: |
| Normal first 30 s | 200 | 1.58 | 1.41 | 4.17 |
| Normal next 30 s | 871 | 1.58 | 1.41 | 4.16 |
| Held snapshot, 30 s | 36,384 | 11.21 | 4.84 | 4.97 |
| After release, 30 s | 1,172 | 11.30 | 4.95 | 4.11 |

At the first one-second sample after release, estimated dead tuples had fallen
to 307 and claim p95 was 3.84 ms. Over the following 29 seconds, table/index sizes
remained exactly flat while approximately 200 jobs/s and 640 heartbeats/s
continued. This is evidence of space reuse and latency recovery in this test,
not proof that vacuum returned disk space: the files stayed much larger than
the control. Summary statistics may differ from the last sample because closing
worker connections flushes their pending Postgres statistics.

Autovacuum was deliberately configured aggressively for this diagnostic. The
observed one-second recovery is not a production recovery-time guarantee. Repeat
with the deployment's actual autovacuum settings and representative workload.
Use [queue health monitoring](MONITORING.md) to distinguish a cleared snapshot
from lingering dead tuples or persistent file growth.
