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

| case                          |       rps | mean ms | p95 ms |
| ----------------------------- | --------: | ------: | -----: |
| nginx single plaintext        | 13,691.31 |   36.45 |  45.04 |
| Tako single plaintext         | 12,675.96 |   39.37 |  50.51 |
| Caddy single plaintext        |  5,980.57 |   83.49 | 129.68 |
| nginx load-balanced plaintext | 12,804.48 |   38.97 |  52.70 |
| Tako load-balanced plaintext  | 10,229.89 |   48.82 |  70.15 |
| Caddy load-balanced plaintext |  5,361.73 |   93.10 | 140.98 |

VM-local high-load headline:

| case                          |  conc | 200 rps | p99 ms | note                       |
| ----------------------------- | ----: | ------: | -----: | -------------------------- |
| nginx single plaintext        |   100 |  27,694 |      9 | best clean low-latency row |
| Tako single plaintext         |   100 |  21,205 |     10 | best Tako low-latency row  |
| Caddy load-balanced plaintext |   100 |  13,683 |     20 | best Caddy row             |
| Tako single plaintext         | 2,500 |  14,379 |    876 | source-sharded overload    |
| Tako single plaintext         | 5,000 |  12,446 |  3,753 | source-sharded overload    |

Findings:

- Tako single-instance proxying was about 7.4% behind nginx and much faster
  than Caddy in this cross-network TLS run.
- The single 2 vCPU VM did not approach 60k-100k clean TLS rps. With TLS and
  same-box load generation, the best clean low-latency row was nginx at 27.7k
  rps; Tako's best low-latency row was 21.2k rps.
- Tako has a built-in 2048 concurrent request cap per client IP. High-load
  benchmarks above that must shard source IPs or apply equivalent limits to the
  comparison proxies.
- Tako's load-balanced path is still a profiling target in the useful latency
  range, and Tako proxy RSS grows sharply under c2500-c10000 overload.
- The released server failed the first channel/workflow benchmark because app
  processes could not use the internal workflow/channel Unix socket. A source
  fix was added in `tako-workflows`; the patched server produced clean 200-only
  feature results.
