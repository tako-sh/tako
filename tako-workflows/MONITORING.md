# Workflow queue monitoring

The existing Tako Prometheus listener (`127.0.0.1:9898/metrics` by default)
exposes workflow claim latency when metrics are enabled. Import the
[Tako dashboard](../grafana/tako-overview.json) to see it. Its server/app filters
include apps that only run workflows.

Claims record their own timings, so scrapes never query the workflow store.
Timers start when a scrape first observes an app's store, and reset when its
runtime is replaced. Stopped apps disappear from the next scrape. Disabled
metrics record no claim timings.

Every sample has `server`, `app` and `outcome` labels. The outcome is `claimed`,
`empty`, or `error`. No database URL, run ID, worker ID, SQL text, or payload is
used as a metric label.

## Metrics

| Metric | Meaning |
| --- | --- |
| `tako_workflow_claim_duration_seconds` | Histogram of the entire storage claim call, including the round trip to the workflow database, its commit, and loading saved steps. This is not enqueue-to-dispatch time. |

## Useful PromQL

Successful claim p95 per app/server:

```promql
histogram_quantile(0.95, sum by (le, server, app) (
  rate(tako_workflow_claim_duration_seconds_bucket{outcome="claimed"}[5m])
))
```

Claims failing against the store:

```promql
sum by (server, app) (rate(tako_workflow_claim_duration_seconds_count{outcome="error"}[5m])) > 0
```
