# Benchmarks

Detailed benchmark harness, raw data, graphs, methodology, VM details, and
analysis live in the dedicated performance repository:

- Repository: <https://github.com/tako-sh/performance>
- Latest results: <https://github.com/tako-sh/performance/blob/main/RESULTS.md>

## TLDR

Latest clean single-VM HTTP/TLS run:

- Tako release: `tako-server 0.0.0-510c153`
- Setup: load generator, proxy, and app all on one 2 vCPU Ubuntu VM
- Result: Tako clearly beats Caddy, but still trails nginx on raw HTTPS reverse
  proxy throughput and most p99 latency rows
- Heavy rows: c5000 `12.2k` Tako 200 RPS vs `17.4k` nginx; c10000 `10.2k`
  vs `14.8k`; c15000 `8.5k` vs `12.0k`; c20000 `6.6k` vs `8.1k`
- Tako stayed clean through c20000: 0 client errors and 0 non-200 responses.
  Nginx had a small non-200/error rate at c20000; Caddy overloaded earlier.
- Channels/workflows improved and are clean through c4000. At c8000, channel
  publish returns 6.3% non-200 responses and workflow enqueue returns 19.7%.
- Main next target: reduce Tako proxy RSS and downstream connection/session
  memory pressure; Tako peaked around 2.25 GiB proxy RSS at c20000

Load-balanced mode is intentionally excluded from the exe-node result set. It
needs a larger or multi-node testbed.
