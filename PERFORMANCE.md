# Performance

Detailed benchmark harness, raw data, graphs, and analysis live in the
dedicated performance repository:

- Repository: <https://github.com/tako-sh/performance>
- Latest report: <https://github.com/tako-sh/performance/blob/main/PERFORMANCE.md>

## 2026-05-31 Latest Release Rerun

Environment:

- Server: Ubuntu 24.04.4 LTS VM, 2 vCPU AMD EPYC 9554P, 7.8 GiB RAM
- Region observed from public geolocation: Tokyo, Japan
- VM-local high-load path: load generator, proxy, and app all on the same VM
- Route: `https://bench.test:18443`, same TLS certificate and app payloads for
  Tako, nginx, and Caddy
- Exact hostnames, public IPs, private network addresses, and user identifiers
  are intentionally omitted from public reports.

Latest run:

- Tako release: `tako-server 0.0.0-d81cc6d`
- HTTP results:
  <https://github.com/tako-sh/performance/tree/main/results/20260531T171211Z/http-vm-local>
- HTTP graphs:
  <https://github.com/tako-sh/performance/blob/main/results/20260531T171211Z/http-vm-local/graphs/README.md>
- Channel/workflow results:
  <https://github.com/tako-sh/performance/tree/main/results/20260531T173340Z/tako-features-vm-local>
- Channel/workflow graphs:
  <https://github.com/tako-sh/performance/blob/main/results/20260531T173340Z/tako-features-vm-local/graphs/README.md>

Clean single-upstream HTTP/TLS rows:

|  conc | nginx 200 rps | nginx p99 | Tako 200 rps | Tako p99 | Caddy 200 rps | Caddy p99 |
| ----: | ------------: | --------: | -----------: | -------: | ------------: | --------: |
| 1,000 |        23,445 |     98 ms |       16,408 |   152 ms |         7,926 |    204 ms |
| 2,500 |        17,824 |    495 ms |       14,916 |   509 ms |         7,055 |  2,236 ms |
| 5,000 |        12,446 |  1,251 ms |       12,977 | 2,647 ms |         6,138 |  4,801 ms |
| 7,500 |         9,984 |  2,511 ms |       11,059 | 4,903 ms |         4,820 |  7,878 ms |

Current takeaway:

- Tako is still slower than nginx in the clean lower-load rows; at c2500 it is
  about 16% behind nginx on successful RPS with similar p99 latency.
- Tako clearly beats Caddy on this VM.
- Tako returns more successful responses than nginx at c5000 and c7500, but
  p99 latency is already in seconds, so those rows are overload behavior.
- The 2 vCPU VM does not reach 60k-100k clean TLS RPS. CPU is saturated across
  heavy rows because load generator, proxy, and app share the same machine.
- Load-balanced mode is excluded for this exe-node result set; it needs a
  larger or multi-node testbed.

Most likely remaining nginx gap:

- Tako still does route-table lookup, app/path ownership cloning, per-IP request
  tracking, image/channel/static handler checks, backend selection, backend
  request accounting, and stricter forwarding-header normalization on the hot
  path. The next likely wins are a lock-free route snapshot, avoiding per-request
  route string clones, profiling the per-IP limiter, and reducing proxy RSS
  under high concurrency.
