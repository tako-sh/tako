# Workflow queue monitoring

The existing Tako Prometheus listener (`127.0.0.1:9898/metrics` by default)
now exposes workflow metrics when metrics are enabled. Import the updated
[Tako dashboard](../grafana/tako-overview.json) to see queue age, claim latency,
dead tuples, table/index growth, transaction age, backend xmin age, and sample
freshness. Its server/app filters include apps that only run workflows.

Claim histograms cover both storage backends. Postgres health is sampled every
30 seconds, plus the time spent sampling, on a separate read-only connection per
active app. Queries have a two-second statement timeout and a 500 ms lock timeout;
connecting has a two-second timeout. Scrapes read cached values and never query
Postgres. Failed queries reconnect on the next sample. Disabled metrics create
no monitoring connection and record no claim timings.

Every metric has `server` and `app` labels. Claim latency also has an `outcome`
label: `claimed`, `empty`, or `error`. Timers start when monitoring first observes
the store, and reset when its runtime is replaced. Stopped apps disappear from
the next scrape.

## Metrics

All names below have the `tako_workflow_` prefix.

| Metric | Meaning |
| --- | --- |
| `claim_duration_seconds` | Histogram of the entire storage claim call, including mutex wait, commit, and loading saved steps. This is not enqueue-to-dispatch time. |
| `runnable_jobs` | Due pending jobs for this app. |
| `oldest_runnable_age_seconds` | Time since the oldest pending job became due, not time since it was created. Future scheduled jobs and indefinitely parked workflows are excluded. |
| `pg_runs_dead_tuples`, `pg_runs_live_tuples` | Estimated tuple counts for the shared runs table. |
| `pg_runs_table_bytes`, `pg_runs_index_bytes` | Shared table and total index size. Normal vacuum may enable reuse without shrinking these files. |
| `pg_runs_autovacuum_count` | Autovacuum count since Postgres statistics were reset. A rising count does not prove tuples are being reclaimed. |
| `pg_oldest_transaction_seconds` | Oldest visible transaction in this database, excluding the sampling connection. |
| `pg_oldest_backend_xmin_age_transactions` | Oldest visible backend snapshot horizon measured in transaction IDs, not seconds. |
| `pg_activity_visibility_complete` | 1 if the role has `pg_read_all_stats` privileges; otherwise activity fields can omit other roles' sessions. |
| `health_sample_success` | 1 for a successful latest sample, 0 for failure or before the first sample. |
| `health_last_success_timestamp_seconds` | Unix time of the last successful sample, initially 0. |

The `pg_runs_*` fields describe the whole table, and `pg_oldest_*` fields describe
the database. Apps sharing the same database/table repeat those values. Do not
sum them across apps or servers. No database URL, run ID, worker ID, SQL text, or
payload is used as a metric label.

Health failures retain the last good gauges. Always check sample success and
freshness before treating those values as current. The collection role is not
automatically granted extra privileges; `pg_read_all_stats` is needed for complete
cross-role activity visibility. Backend xmin does not cover replication slots or
prepared transactions, so use database-level monitoring for those retention
sources as well.

## Useful PromQL

Successful claim p95 per app/server:

```promql
histogram_quantile(0.95, sum by (le, server, app) (
  rate(tako_workflow_claim_duration_seconds_bucket{outcome="claimed"}[5m])
))
```

Health sampling needs investigation when either of these remains true for several
sampling intervals:

```promql
tako_workflow_health_sample_success == 0
```

```promql
time() - tako_workflow_health_last_success_timestamp_seconds > 120
```

For backlog alerts, choose an age threshold from the workflow's dispatch SLO and
pair it with `runnable_jobs > 0`. For vacuum trouble, correlate rising dead tuples
and table/index size with old transactions or backend xmin and rising claim
latency. An old transaction alone does not prove it is blocking cleanup.

See the [recovery benchmark](POSTGRES_BENCHMARK.md) for a reproducible pinned
snapshot followed by continued traffic and normal autovacuum recovery.
