---
description: Run and publish Tako proxy performance benchmarks
---

$ARGUMENTS

# Performance Benchmark

Run a repeatable Tako performance benchmark, update the public performance
report, and leave the main Tako repo with only a short TLDR link.

Use the dedicated performance repository for harness code, raw data, graphs, and
the detailed report. Use this repo only for the high-level `BENCHMARKS.md`
summary.

This command is an end-to-end workflow. Do not stop at a plan or raw benchmark
output unless blocked: prepare the VM, run the tests, inspect the results, fix
obvious harness or simple Tako issues, rerun when needed, update reports,
sanitize, validate, commit, and push.

## Inputs

The user should provide a benchmark server for each run. Do not assume a server
from a previous chat still exists or is still accessible.

The user may provide:

- benchmark VM SSH host;
- optional target IP or local-only mode;
- target Tako release or branch;
- whether to compare latest release, a patched build, or both;
- whether to run only HTTP proxy tests or also channel/workflow tests;
- whether a larger/multi-node server is available for load-balancer tests.

If the VM host is not clear, ask for it before doing any remote work. If the
target release is not clear, use the latest published Tako release unless the
user explicitly asks for a local/patched build.

## Defaults

- Performance repo: `~/github/tako-performance`
- Public repo: `git@github.com:tako-sh/performance.git`
- Main repo summary: `BENCHMARKS.md`
- Detailed report: `RESULTS.md`
- Benchmark server: supplied by the user for that run; never reuse an old host
  from memory or thread history without the user confirming it.
- Timed HTTP path: VM-local, HTTPS, HTTP/1.1, same certificate and route for
  nginx, HAProxy, Envoy, Caddy, and Tako.
- Load-balanced mode: skip on the small exe-node/2 vCPU VM. Only run LB on a
  larger or multi-node testbed.
- History policy: KISS. Keep the latest authoritative report and raw result
  set. Keep only notable older runs when they explain an important correction.

## Non-Negotiable Rules

- Do not expose sensitive data in public files. Never commit real SSH hosts,
  public IPs, private IPs, local usernames, local absolute paths, peer names,
  tokens, secrets, or Tailscale identifiers.
- It is okay to name the server provider or VM platform, such as "exe.dev", if
  that does not reveal the exact host, account, peer, or private network.
- Real hostnames may be used in local commands, but public docs must use
  placeholders such as `<ssh-host>` and controlled benchmark names such as
  `bench.test`.
- Before committing public reports, scan for sensitive strings.
- Keep all proxies under equivalent conditions: same URL, Host, SNI, TLS cert,
  upstream app behavior, warmup, duration, source IP set, and concurrency list.
- Keep request/connection protection comparable without making the limiter the
  benchmark. Tako's released default enforces 2048 active requests per derived
  client IP, so high-concurrency runs must use the documented 16 loopback source
  IPs. Configure nginx and HAProxy with high per-IP connection guards. Caddy
  must use the benchmark repo's Caddy binary built with
  `github.com/mholt/caddy-ratelimit`; keep its ceiling high enough not to
  impose a different RPS bottleneck. Envoy must use the benchmark repo config
  with high local-rate-limit and raised cluster circuit-breaker thresholds.
- Do not compare Tako load-balanced mode on the 2 vCPU exe-node; it mostly
  measures process contention, not load-balancer quality.
- Do not treat high-load client errors as proxy failures until error samples
  prove the source. Check `error_kinds` and `error_samples`.
- Use the fixed load generator behavior: `REQUEST_TIMEOUT=60s`, sampled error
  messages, and per-host connection cap equal to concurrency.

## Phase 1 — Preflight

1. Check both worktrees:

```bash
git -C ~/github/tako status --short --branch
git -C ~/github/tako-performance status --short --branch
```

2. Check local machine load if the client will generate traffic. For VM-local
   runs this is informational only.

3. Check benchmark VM health before running:

```bash
ssh <ssh-host> uptime
ssh <ssh-host> free -h
ssh <ssh-host> ps -eo pid,ppid,pcpu,pmem,comm,args --sort=-pcpu
ssh <ssh-host> 'systemctl show tako-server -p LimitNOFILE 2>/dev/null || true'
```

Abort or explain before continuing if another process is consuming enough CPU or
memory to distort results.

Also confirm the app processes used by the benchmark inherit a high file
descriptor limit. A low app-side `nofile` limit can turn high-concurrency rows
into upstream `502`s that look like proxy or SQLite failures.

4. Capture non-sensitive server details for the report:

```bash
ssh <ssh-host> 'uname -a; lsb_release -a 2>/dev/null || cat /etc/os-release; nproc; free -h; df -h /'
```

If region or ping matters, measure it for the current server. Report the
provider/platform, sanitized region, and latency summary, but not exact
hostnames or IPs.

5. Confirm benchmark tools are current and clean:

```bash
cd ~/github/tako-performance
go test ./...
go build ./cmd/...
bash -n scripts/run-vm-local-http-benchmarks.sh \
  scripts/run-vm-local-tako-feature-benchmarks.sh \
  scripts/run-http-benchmarks.sh \
  scripts/run-tako-feature-benchmarks.sh \
  scripts/remote/start-metrics.sh \
  scripts/remote/sample-metrics.sh
```

## Phase 2 — Prepare VM

Sync the performance repo to the VM:

```bash
cd ~/github/tako-performance
BENCH_VM=<ssh-host> ./scripts/sync-to-vm.sh
```

Prefer the published Tako release for release benchmarks. Build locally only
when intentionally benchmarking an unreleased patch, and label it clearly in
`RESULTS.md`.

Install the benchmarked release on the VM and pass its absolute path as
`TAKO_SERVER_BIN`. Verify the running process version before the final run so a
stale `/usr/local/bin/tako-server` cannot silently become the measured binary.

For feature benchmarks, run separate rows for accepted enqueue/publish pressure
and for worker execution pressure when investigating failures. The combined row
is useful as an end-to-end product test, but it mixes app HTTP handling, SDK
internal-socket RPC, SQLite persistence, and workflow worker writes.

If the user asked to benchmark a release that is not available yet, wait for or
verify the release before running the final benchmark. A patched/local build can
be used for diagnosis, but do not present it as a release result.

## Phase 3 — Run HTTP Proxy Benchmark

Use VM-local load generation for the small exe-node so public internet latency
and the laptop do not dominate the result.

The performance repo configs are expected to include nginx `limit_conn`,
HAProxy stick-table connection tracking, Envoy local rate limiting, and the
Caddy rate-limit module so Tako is not the only proxy doing per-client
protection work on the hot path.

For controlled high-concurrency runs, use the documented 16 loopback source IPs.
The production default is 2048 active requests per client IP; a single source IP
can fail at exactly 2048 with `429`, which measures the safety limiter instead
of proxy throughput. If a future released build supports
`TAKO_MAX_REQUESTS_PER_IP`, it can also be passed, but do not depend on it for
current release comparisons.

```bash
cd ~/github/tako-performance
BENCH_VM=<ssh-host> \
TAKO_SERVER_BIN=/opt/tako-performance/bin/<tako-server-release> \
SOURCE_IPS='127.0.0.2,127.0.0.3,127.0.0.4,127.0.0.5,127.0.0.6,127.0.0.7,127.0.0.8,127.0.0.9,127.0.0.10,127.0.0.11,127.0.0.12,127.0.0.13,127.0.0.14,127.0.0.15,127.0.0.16,127.0.0.17' \
PROXIES='nginx haproxy envoy tako caddy' \
MODES=single \
ENDPOINTS=plaintext \
CONCURRENCY_LIST='1000 2500 5000 7500 10000 15000 20000' \
WARMUP=10s \
DURATION=30s \
REQUEST_TIMEOUT=60s \
METRICS_INTERVAL=1 \
METRICS_CONNECTIONS=1 \
COOLDOWN_SECONDS=10 \
./scripts/run-vm-local-http-benchmarks.sh
```

Keep Caddy last in the ordered proxy list for the small-VM run. Its overload
rows can leave the VM hot enough to bias the immediately following row.

If the user mainly wants overload behavior, run the full list above. If the user
mainly wants production capacity, add a separate lower-load/SLO-oriented run and
report where p99 and errors cross the agreed threshold.

## Phase 4 — Run Channel/Workflow Benchmark

Run this separately from the proxy comparison:

```bash
cd ~/github/tako-performance
BENCH_VM=<ssh-host> \
TAKO_SERVER_BIN=/opt/tako-performance/bin/<tako-server-release> \
SOURCE_IPS='127.0.0.2,127.0.0.3,127.0.0.4,127.0.0.5,127.0.0.6,127.0.0.7,127.0.0.8,127.0.0.9,127.0.0.10,127.0.0.11,127.0.0.12,127.0.0.13,127.0.0.14,127.0.0.15,127.0.0.16,127.0.0.17' \
CONCURRENCY_LIST='500 1000 2000 4000 8000' \
WARMUP=10s \
DURATION=30s \
REQUEST_TIMEOUT=60s \
METRICS_INTERVAL=1 \
METRICS_CONNECTIONS=1 \
COOLDOWN_SECONDS=10 \
./scripts/run-vm-local-tako-feature-benchmarks.sh
```

## Phase 5 — Inspect Results

For every run, inspect:

- 200 RPS;
- total RPS;
- p50/p95/p99 latency;
- non-200 percentage and status counts;
- client error percentage;
- `error_kinds` and `error_samples`;
- CPU and RAM graphs;
- proxy RSS and loadgen RSS;
- max TLS connections.

Use raw JSON/CSV evidence. Do not rely only on the summary graph.

Known local profiling notes to avoid repeating:

- High proxy RSS at large downstream connection counts is not OpenSSL alone. A
  local macOS 5k HTTPS keepalive control measured roughly 191 MB RSS for raw
  Tokio + OpenSSL, roughly 356 MB for a Pingora-only HTTPS responder with no
  upstream proxying, and roughly 524-558 MB for full Tako after deploy.
- A follow-up Linux control on the 2 vCPU exe.dev VM confirmed the useful
  split: raw Tokio + OpenSSL was ~136.6 MB at 5k HTTPS keepalive connections,
  a Pingora-only HTTPS responder was ~341.4 MB, a fixed-upstream Pingora
  reverse proxy was ~535.3 MB, and full Tako was ~549.0 MB. Full Tako's
  post-deploy baseline was ~51.9 MB. The large 341->535 MB jump is the generic
  Pingora reverse-proxy path, not Tako routing/LB. Tako's overhead above the
  comparable fixed Pingora reverse proxy was only ~14 MB at 5k live
  connections in that control.
- A fixed-upstream Pingora reverse-proxy RPS control on the same small VM was
  not faster than Tako: it measured roughly 7.1k RPS at c10000, 4.7k at c15000,
  and 1.6k at c20000 with 502s. Treat this as diagnostic evidence that the
  remaining nginx gap is not explained by Tako route lookup, load-balancer
  lookup, or limiter accounting alone.
- A local patched Tako build with a cheaper existing-IP limiter path did not
  produce a stable headline win. Keep the cleanup if it stays simple, but do
  not claim it as the reason for improved results without repeatable A/B rows.
- Do not over-interpret the local macOS post-deploy baseline. `vmmap -summary`
  showed large framework/IOAccelerator mappings, so Linux VM RSS is the source
  of truth for published numbers.
- `SSL_MODE_RELEASE_BUFFERS` did not materially reduce retained RSS in profiling
  runs. `MIMALLOC_PURGE_DELAY=0` repeatedly lowered post-burst retained Tako RSS
  on the Linux exe.dev VM, but it did not reduce the live c20000 RSS peak that
  appears in published benchmark rows. Do not set it as the production service
  default unless prod data shows retained RSS matters more than the extra purge
  churn risk.
- Disabling downstream keepalive, or setting the keepalive request limit to
  `0`/`1`, hides much of the RSS but is not comparable to nginx, HAProxy,
  Envoy, or Caddy and hurts RPS. Keep the bounded keepalive request limit
  instead.
- A temporary Pingora 0.8 H1 body-buffer patch that reduced the internal
  `BODY_BUFFER_SIZE` from 64 KiB to 4 KiB did not improve the raw fixed-proxy
  c20000 RSS/RPS control. Do not vendor or fork Pingora for that change without
  stronger evidence.

Helpful command:

```bash
jq '{name, concurrency, request_timeout_sec, requests, errors, requests_per_sec, latency_ms, status_counts, error_kinds, error_samples}' results/<timestamp>/http-vm-local/*.json
```

Classify results honestly:

- Saturation result: CPU near 100%, useful for maximum-pressure behavior.
- Capacity result: latency and error rates still within an explicit SLO.
- Harness artifact: error samples point to loadgen timeout, local address
  exhaustion, file descriptors, or other client-side limits.

## Phase 5b — Fix Obvious Issues And Rerun

If inspection shows a benchmark-harness artifact, fix the harness in
`~/github/tako-performance`, validate it, sync it to the VM, and rerun the
affected cases before updating `RESULTS.md`.

If inspection shows a simple, high-confidence Tako issue, fix it in
`~/github/tako` with tests where required, commit it, wait for or produce the
intended benchmark build, and rerun the affected cases. Do not publish a result
whose main conclusion is based on a known-bad harness or a known-fixed local
bug unless the report clearly labels it as superseded/diagnostic.

Examples of issues to fix before publishing:

- client errors caused by load-generator timeouts, file-descriptor limits, or
  local source-port exhaustion;
- metrics graphs distorted by stale samplers or negative process CPU deltas;
- process metrics accidentally including unrelated system services, such as the
  VM's default nginx;
- wrong proxy mode, URL, Host/SNI, TLS, source IPs, or timeout settings;
- an obvious Tako hot-path regression already fixed locally and awaiting a
  release.

## Phase 6 — Update Reports

In `~/github/tako-performance`:

1. Update `RESULTS.md` as the latest authoritative report.
2. Keep the report public-safe and concise.
3. Include:
   - release/build under test;
   - provider/platform, VM shape, OS, region, and ping summary without exact
     host/IP;
   - methodology;
   - graph links;
   - raw result directory links;
   - HTTP proxy results;
   - channel/workflow results when run;
   - known limitations;
   - next performance targets.
4. Prefer latest-only. Do not add a large history unless the user asks. Keep a
   short note for notable invalidated/superseded runs only when it prevents
   confusion.

In `~/github/tako`:

1. Update `BENCHMARKS.md` with only a TLDR and links to the performance repo.
2. Do not duplicate full tables or raw data in the main repo.

## Phase 7 — Sanitize

Before committing, scan both repos:

```bash
rg -n "workbench|exe\\.xyz|/Users/|tailscale|Tailscale|exedev|ssh-rsa|BEGIN .*KEY|token|secret|password" \
  ~/github/tako-performance/README.md \
  ~/github/tako-performance/RESULTS.md \
  ~/github/tako-performance/results \
  ~/github/tako/BENCHMARKS.md
```

Investigate any hit. Keep legitimate generic words only when they do not expose
sensitive details.

## Phase 8 — Verify, Commit, Push

Run:

```bash
cd ~/github/tako-performance
go test ./...
go build ./cmd/...
git diff --check

cd ~/github/tako
git diff --check
```

Commit performance repo changes:

```bash
cd ~/github/tako-performance
git add README.md RESULTS.md cmd scripts results
git commit -m "perf(benchmarks): update performance results"
git push origin main
```

Commit main repo summary:

```bash
cd ~/github/tako
git add BENCHMARKS.md
git commit -m "docs(benchmarks): update performance summary"
git push origin master
```

If hooks run broader checks, let them finish. Do not commit with known failures
unless the user explicitly approves.

## Final Response

Report:

- what was run;
- latest result directory and report links;
- headline numbers;
- whether client errors are real proxy failures or harness/client artifacts;
- what changed in reports or harness;
- validation commands;
- commit hashes and push status.
