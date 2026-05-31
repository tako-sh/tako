# Performance

Detailed benchmark harness, raw data, graphs, and analysis live in the
dedicated performance repository:

- Repository: <https://github.com/tako-sh/performance>
- Latest report: <https://github.com/tako-sh/performance/blob/main/PERFORMANCE.md>

## 2026-05-31 Latest Clean Rerun

Environment:

- Server: Ubuntu 24.04.4 LTS VM, 2 vCPU AMD EPYC 9554P, 7.8 GiB RAM
- Region observed from public geolocation: Tokyo, Japan
- VM-local high-load path: load generator, proxy, and app all on the same VM
- Route: `https://bench.test:18443`, same TLS certificate and app payloads for
  Tako, nginx, and Caddy
- Exact hostnames, public IPs, private network addresses, and user identifiers
  are intentionally omitted from public reports.

Latest run:

- Tako release: `tako-server 0.0.0-850a9e2`
- HTTP results:
  <https://github.com/tako-sh/performance/tree/main/results/20260531T193211Z/http-vm-local>
- HTTP graphs:
  <https://github.com/tako-sh/performance/blob/main/results/20260531T193211Z/http-vm-local/graphs/README.md>
- Channel/workflow results:
  <https://github.com/tako-sh/performance/tree/main/results/20260531T195359Z/tako-features-vm-local>
- Channel/workflow graphs:
  <https://github.com/tako-sh/performance/blob/main/results/20260531T195359Z/tako-features-vm-local/graphs/README.md>

Clean single-upstream HTTP/TLS rows:

|  conc | nginx 200 rps | nginx p99 | Tako 200 rps | Tako p99 | Caddy 200 rps | Caddy p99 |
| ----: | ------------: | --------: | -----------: | -------: | ------------: | --------: |
| 1,000 |        19,245 |    145 ms |       14,476 |   158 ms |         6,835 |    247 ms |
| 2,500 |        14,092 |    652 ms |       13,029 |   585 ms |         6,066 |  2,608 ms |
| 5,000 |        11,696 |  1,388 ms |        8,023 | 3,108 ms |         5,318 |  5,460 ms |
| 7,500 |         9,485 |  2,885 ms |        9,264 | 6,893 ms |         2,203 |  7,591 ms |

Current takeaway:

- Tako is still behind nginx in lower-load clean rows, but clearly ahead of
  Caddy on this VM.
- At c2500, Tako is within about 8% of nginx on successful RPS and has slightly
  better p99 in this run.
- c7500+ is overload behavior on this 2 vCPU VM. CPU is saturated and p99 is
  already in seconds.
- The 2 vCPU VM does not reach 60k-100k clean TLS RPS because load generator,
  proxy, and app share the same machine.
- Load-balanced mode is excluded for this exe-node result set; it needs a
  larger or multi-node testbed.

Fix shipped during this round:

- `850a9e2c perf(proxy): scale upstream keepalive pool` changes Pingora's
  upstream keepalive pool from 256 total to 256 per proxy thread.

Most likely remaining nginx gap:

- Tako still does more product work than the static nginx config: request
  path/host ownership, route lookup, per-IP request tracking, image/channel/static
  handler checks, backend selection/accounting, upstream peer construction, and
  stricter forwarding-header normalization.
- The latest graphs show Tako proxy RSS is materially higher than nginx at
  heavy concurrency. Reducing downstream connection/session memory pressure is
  the next best target.
