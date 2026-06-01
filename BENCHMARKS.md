# Benchmarks

Detailed benchmark harness, raw data, graphs, methodology, VM details, and
analysis live in the dedicated performance repository:

- Repository: <https://github.com/tako-sh/performance>
- Latest results: <https://github.com/tako-sh/performance/blob/main/RESULTS.md>

## TLDR

Latest clean single-VM HTTP/TLS run:

- Tako release: `tako-server 0.0.0-770afb0`
- Note: the experimental memory-counter/TLS-cache patch in `770afb0` was
  reverted after this run because it did not produce a reliable benchmark win.
- Setup: load generator, proxy, and app all on one 2 vCPU Ubuntu VM
- Result: Tako clearly beats Caddy, but still trails nginx on raw HTTPS reverse
  proxy throughput and most p99 latency rows
- Heavy rows: c5000 `12.4k` Tako 200 RPS vs `17.2k` nginx; c10000 `10.2k`
  vs `13.0k`; c15000 `8.2k` vs `12.7k`; c20000 `6.8k` vs `13.1k`
- Tako stayed clean through c20000: 0 client errors and 0 non-200 responses.
  Caddy overloaded; nginx had small 500/error rates at c10000/c15000 in this
  run.
- Channels/workflows are clean through c4000. At c8000, channel publish returns
  6.0% non-200 responses and workflow enqueue returns 23.8%.
- Main next target: reduce Tako proxy RSS, downstream connection/session memory
  pressure, and p99 latency; Tako peaked around 2.5 GiB proxy RSS at c20000

Load-balanced mode is intentionally excluded from the exe-node result set. It
needs a larger or multi-node testbed.
