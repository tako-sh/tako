---
title: "Self-Hosted App Monitoring with Prometheus: VPS Latency, Cold Starts, and Health"
seoTitle: "Self-Hosted App Monitoring with Prometheus"
date: "2026-11-02T03:15"
description: "Monitor Tako apps on your VPS with Prometheus: scrape local metrics, query request latency, investigate cold starts, and inspect instance health."
image: d7c0252005dc
imageAlt: "A coral octopus watches Prometheus latency charts and adjusts server controls in a retro mission-control room."
---

Your app deployed successfully. An hour later, someone says the dashboard feels slow. The process is running, the homepage loads, and the server still answers SSH. That narrows the mystery slightly. It does not tell you where the time went.

Tako exposes Prometheus metrics for its proxy and app lifecycle. We can use those to separate backend request latency, cold starts, and instance health, then follow the evidence into logs. We'll start with one VPS and a local Prometheus process. No dashboard template is required to ask useful questions.

## Scrape the local metrics endpoint

On a normal Tako server, metrics listen on `127.0.0.1:9898`. The [operations reference](/docs/operations/#metrics) lists the metric families; the [listener implementation](https://github.com/tako-sh/tako/blob/master/tako-server/src/proxy/server.rs) confirms that the address is loopback. From a shell on the VPS, check it:

```bash
curl -fsS http://127.0.0.1:9898/metrics
```

Look for `tako_` metric names and their labels. Some series appear only after the corresponding activity happens, so a quiet server can have fewer samples than you expect. Make a request to a real backend route, then inspect the endpoint again. A static file request exercises a different path.

The server flag `--metrics-port` selects another port; `0` disables the endpoint and request metrics. Changing that port does not make the listener public. Keep your [server management setup](/docs/servers/) separate from monitoring: Tako's private management listener and its local metrics listener serve different purposes.

For this example, install Prometheus on the same VPS using its [getting started guide](https://prometheus.io/docs/prometheus/latest/getting_started/). Create `prometheus.yml` in a directory where you'll run the Prometheus binary:

```yaml
global:
  scrape_interval: 15s

scrape_configs:
  - job_name: tako
    metrics_path: /metrics
    static_configs:
      - targets: ["127.0.0.1:9898"]
```

Validate the configuration and start a local inspection session:

```bash
./promtool check config prometheus.yml
./prometheus --config.file=prometheus.yml \
  --web.listen-address=127.0.0.1:9090
```

These commands assume the downloaded `prometheus` and `promtool` binaries are in the current directory. The [Prometheus configuration reference](https://prometheus.io/docs/prometheus/latest/configuration/configuration/) explains scrape jobs, and its [server flags](https://prometheus.io/docs/prometheus/latest/command-line/prometheus/) document the explicit web-listener binding. For an ongoing installation, supervise Prometheus and choose its storage location as part of your host operations.

If you are browsing from your laptop, forward the UI through an existing SSH connection:

```bash
ssh -N -L 9090:127.0.0.1:9090 ubuntu@your-vpn-host
```

Replace the user and VPN hostname with your server's actual administrator connection, then open `http://127.0.0.1:9090` locally. This example assumes port `9090` is free on your laptop.

```d2
direction: right

vps: "Your VPS" {
  app: "App instances"
  tako: "Tako proxy\nand lifecycle metrics" {
    style.fill: "#E88783"
  }
  metrics: "127.0.0.1:9898\n/metrics"
  prometheus: "Prometheus\n127.0.0.1:9090" {
    style.fill: "#9BC4B6"
  }
  tako -> app: "backend requests"
  tako -> metrics: "exposes samples"
  prometheus -> metrics: "scrapes"
}

laptop: "Your laptop"
laptop -> vps.prometheus: "SSH-forwarded UI"
```

This keeps the scrape in the host's network namespace. A Prometheus container has its own loopback address; the same target configuration there would point inside that container. Our example runs directly on the VPS so the address means exactly what it says.

Start with this query:

```promql
up{job="tako"}
```

A value of `1` means the scrape succeeded; `0` means it failed. That is [Prometheus target health](https://prometheus.io/docs/concepts/jobs_instances/), not an application readiness check. A healthy metrics endpoint can describe an unhealthy app.

## Read latency alongside request volume

Tako's [metric definitions](https://github.com/tako-sh/tako/blob/master/tako-server/src/metrics.rs) give each family a `server` label. App metrics also carry `app`; copy actual values from the endpoint rather than guessing their spelling. Keep those dimensions when comparing hosts:

| Metric | Type | Useful question |
| --- | --- | --- |
| `tako_http_requests_total` | Counter, with `status` class | Is backend request volume or the 5xx rate changing? |
| `tako_http_request_duration_seconds` | Histogram | How long is the measured proxy request phase? |
| `tako_upstream_request_duration_seconds` | Histogram | How long until the upstream returns response headers? |
| `tako_http_active_connections` | Gauge | How much measured app request activity is in flight? |

For requests per second, grouped by server, app, and response class:

```promql
sum by (server, app, status) (
  rate(tako_http_requests_total{job="tako"}[5m])
)
```

The `status` label contains `2xx`, `3xx`, `4xx`, `5xx`, or `other`, rather than individual codes. Add `status="5xx"` to the selector when investigating server errors. Use `rate()` before aggregation so counter resets are handled per series.

For an estimated p95 request duration in seconds:

```promql
histogram_quantile(
  0.95,
  sum by (server, app, le) (
    rate(tako_http_request_duration_seconds_bucket{job="tako"}[5m])
  )
)
```

The `le` label preserves classic histogram bucket boundaries. To compare upstream timing, run the same expression with `tako_upstream_request_duration_seconds_bucket`. [Prometheus documents this aggregation pattern](https://prometheus.io/docs/prometheus/latest/querying/functions/). Quantiles estimate a distribution; subtracting two p95 values does not calculate per-request proxy overhead.

Read volume before interpreting a percentile. A quiet app may have insufficient observations for a useful five-minute estimate. A missing series or an undefined percentile is not evidence of zero latency.

There is another important boundary in Tako's current implementation: the request timer starts after backend resolution succeeds. Static responses, ACME requests, unmatched hosts, cold-start waiting, and errors returned before a backend is selected do not pass through that timer. The [proxy request path](https://github.com/tako-sh/tako/blob/master/tako-server/src/proxy/service/mod.rs) makes that scope explicit. These histograms are not full browser navigation timings or a count of every public error response.

If upstream timing rises with request timing, investigate application work, database access, and external calls. If the two diverge, inspect request handling and response transfer too. Use the measurements to choose the next question, then read logs for the specific request.

## Separate cold starts from instance failures

An app configured for on-demand operation can have no running instances between visits. The next request waits for startup. That can affect the user's first response even when steady-state backend latency looks excellent. The [scale-to-zero post](/blog/scale-to-zero-without-containers/) explains the lifecycle; here we want to observe it.

| Metric | Meaning |
| --- | --- |
| `tako_cold_starts_total` | Recorded cold-start events, including successful and failed completion paths |
| `tako_cold_start_duration_seconds` | Startup duration observations for those paths |
| `tako_cold_start_failures_total` | Failures partitioned by `reason` |
| `tako_instances_running` | Current running instance count per app |
| `tako_instance_health` | Health per app instance: `1` healthy, `0` unhealthy |

Inspect recent startup failures by reason:

```promql
sum by (server, app, reason) (
  increase(tako_cold_start_failures_total{job="tako"}[15m])
)
```

Then inspect current state directly:

```promql
tako_instances_running{job="tako"}
```

```promql
tako_instance_health{job="tako"}
```

With Prometheus's default label handling, the health metric's exported app-instance label becomes `exported_instance`, because Prometheus supplies its own scrape-target `instance` label. The `server` and `app` labels still distinguish the application. Look at the labels returned by your actual scrape before writing selectors.

Zero running instances can be expected in on-demand mode. An unhealthy instance needs investigation, while a removed instance's health series is removed rather than permanently left at zero. Check the desired operating mode and current state together:

```bash
tako status --json
tako logs --env production --days 1 --json
```

Run those from the relevant project and inspect failures or truncation. Correlate the event with [release history and deployment state](/docs/deployment/), then reproduce the failing public request. Metrics locate the symptom; the [operations guide](/docs/operations/) connects it to finite logs and verification.

Start with a successful scrape, a real backend request, and one latency query. Add lifecycle metrics as you investigate startup behavior. Your first useful monitoring setup is the one that helps explain the next slow request.
