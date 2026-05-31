# Performance

Detailed benchmark harness, raw data, and analysis live in the dedicated
performance repository:

- Repository: <https://github.com/tako-sh/performance>
- Baseline report: <https://github.com/tako-sh/performance/blob/main/PERFORMANCE.md>

## 2026-05-31 Baseline

Environment:

- Server: Ubuntu 24.04.4 LTS VM, 2 vCPU AMD EPYC 9554P, 7.8 GiB RAM
- Region observed from VM public address: Tokyo, Japan
- Load generator: macOS laptop over Tailscale
- VM-local high-load pass: load generator, proxy, and app all on the same VM,
  with 16 loopback source IPs for high-concurrency runs
- Route: `https://bench.test:18443`, same TLS certificate and app payloads for
  Tako, nginx, and Caddy
- Exact hostnames, public IPs, private Tailscale IPs, and user identifiers are
  intentionally omitted from the public report.

Headline 500-concurrency HTTP/TLS results:

| case                   |       rps | mean ms | p95 ms |
| ---------------------- | --------: | ------: | -----: |
| nginx single plaintext | 13,691.31 |   36.45 |  45.04 |
| Tako single plaintext  | 12,675.96 |   39.37 |  50.51 |
| Caddy single plaintext |  5,980.57 |   83.49 | 129.68 |

VM-local high-load headline:

| case                   |  conc | 200 rps | p99 ms | note                       |
| ---------------------- | ----: | ------: | -----: | -------------------------- |
| nginx single plaintext |   100 |  27,694 |      9 | best clean low-latency row |
| Tako single plaintext  |   100 |  21,205 |     10 | best Tako low-latency row  |
| Caddy single plaintext |   100 |  12,128 |     21 | best Caddy single row      |
| Tako single plaintext  | 2,500 |  14,379 |    876 | source-sharded overload    |
| Tako single plaintext  | 5,000 |  12,446 |  3,753 | source-sharded overload    |

Findings:

- Tako single-instance proxying was about 7.4% behind nginx and much faster
  than Caddy in this cross-network TLS run.
- The single 2 vCPU VM did not approach 60k-100k clean TLS rps. With TLS and
  same-box load generation, the best clean low-latency row was nginx at 27.7k
  rps; Tako's best low-latency row was 21.2k rps.
- Tako has a built-in 2048 concurrent request cap per client IP. High-load
  benchmarks above that must shard source IPs or apply equivalent limits to the
  comparison proxies.
- Load-balanced rows are intentionally excluded for this 2 vCPU exe-node
  report; Tako proxy RSS grows sharply under c2500-c10000 overload.
- The released server failed the first channel/workflow benchmark because app
  processes could not use the internal workflow/channel Unix socket. A source
  fix was added in `tako-workflows`; the patched server produced clean 200-only
  feature results.

## 2026-05-31 Released Active-Set Rerun

Detailed report:
<https://github.com/tako-sh/performance/blob/main/PERFORMANCE.md#released-active-set-rerun>

Raw data and graphs:

- HTTP results:
  <https://github.com/tako-sh/performance/tree/main/results/20260531T113110Z/http-vm-local>
- HTTP graph index:
  <https://github.com/tako-sh/performance/blob/main/results/20260531T113110Z/http-vm-local/graphs/README.md>
- Channel/workflow results:
  <https://github.com/tako-sh/performance/tree/main/results/20260531T120513Z/tako-features-vm-local>
- Channel/workflow graph index:
  <https://github.com/tako-sh/performance/blob/main/results/20260531T120513Z/tako-features-vm-local/graphs/README.md>

This rerun used released `tako-server 0.0.0-1c29253`, after the
active-set routing change. It skipped low concurrency and ran VM-local HTTPS
load at c2500, c5000, c7500, c10000, and c15000 with 16 loopback source IPs,
10s warmups, and 30s measured windows. Load-balanced rows are excluded for
this VM because four upstream app processes mostly measure CPU contention on a
2 vCPU box.

Released-rerun HTTP/TLS headline:

| case         |   conc | 200 rps | p99 ms | client errors | note                                |
| ------------ | -----: | ------: | -----: | ------------: | ----------------------------------- |
| nginx single |  2,500 |  18,781 |    370 |         0.00% | clean high-load leader              |
| Tako single  |  2,500 |  15,280 |    794 |         0.00% | clean, below nginx                  |
| Caddy single |  2,500 |   7,205 |  2,152 |         0.00% | far behind                          |
| Tako single  |  5,000 |  13,371 |  3,098 |         0.00% | overload, higher 200 rps than nginx |
| nginx single |  5,000 |  12,729 |  1,038 |         0.00% | lower latency than Tako             |
| Tako single  | 10,000 |  10,964 |  7,164 |         0.09% | overload survivability              |
| nginx single | 10,000 |   3,294 |  6,626 |         1.92% | overload/failure mode               |
| nginx single | 15,000 |   2,401 |  9,807 |         8.38% | failure mode                        |
| Tako single  | 15,000 |      77 |  9,856 |        94.31% | failure mode                        |

Current takeaway:

- A follow-up code fix after this run changes Tako's Pingora service from the
  default one thread to host parallelism and raises the upstream keepalive pool
  to 256. These published numbers are pre-fix and should be rerun before
  treating the nginx gap as final.
- Load-balanced mode is deferred until a larger or multi-node testbed; this VM
  is useful for single-upstream throughput and failure-mode behavior only.
- Tako single can produce more raw 200 rps than nginx in overload rows, but
  p99 latency is worse. Treat those rows as survivability data, not a steady
  operating point.
- The released server now passes channel/workflow benchmarks cleanly through
  c4000; c8000 enters failure mode with 502/503 responses.
