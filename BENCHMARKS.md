# Benchmarks

Detailed benchmark harness, raw data, graphs, methodology, VM details, and
analysis live in the dedicated performance repository:

- Repository: <https://github.com/tako-sh/performance>
- Latest results: <https://github.com/tako-sh/performance/blob/main/RESULTS.md>

## TLDR

Latest clean single-VM HTTP/TLS run:

- Tako release: `tako-server 0.0.0-339c020`
- Setup: load generator, proxy, and app all on one 2 vCPU Ubuntu VM
- Result: Tako clearly beats Caddy, but still trails nginx on raw HTTPS reverse
  proxy throughput and p99 latency
- Heavy rows: c5000 `12.2k` Tako 200 RPS vs `17.2k` nginx; c10000 `9.9k` vs
  `14.8k`; c20000 `6.9k` vs `9.4k`
- Tako stayed clean through c20000 in the final run: 0 client errors and 0
  non-200 responses
- Channels/workflows are good through c4000, but not excellent overall: c8000
  overload returns 14-20% non-200 responses
- Main next target: reduce Tako proxy RSS and downstream connection/session
  memory pressure; Tako peaked around 2.6 GiB proxy RSS at c20000

Load-balanced mode is intentionally excluded from the exe-node result set. It
needs a larger or multi-node testbed.
